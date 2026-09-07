use alloc::vec::Vec;

use evm2_macros::instruction;

use crate::{
    BaseEvmConfig, EvmConfig, ExecutionConfig, SpecId,
    env::BlockEnvExt,
    interpreter::{Host, InstrStop, Interpreter, Message, Word, op},
    test_utils::{RunConfig, TestHost, TestInterpreter, TestTypes, legacy_bytecode, push},
    version::OpcodeConfig,
};

const ADD_OPCODE: u8 = 0x0c;
const DYNAMIC_GAS_OPCODE: u8 = 0x0d;
const NO_STACK_PREAMBLE_OPCODE: u8 = 0x0e;
const CONCRETE_EQ_OPCODE: u8 = 0x0f;
const TYPE_BOUND_OPCODE: u8 = 0x1d;
const ASSOC_BOUND_OPCODE: u8 = 0x1e;

#[instruction]
fn macro_add([a, b]: [Word]) -> out {
    *out = *a + *b;
}

#[instruction(dynamic_gas)]
fn macro_dynamic_gas(cx: _) -> Result<out> {
    cx.gas.spend(3)?;
    *out = Word::from(0xda_u64);
}

#[instruction(no_stack_preamble)]
fn macro_no_stack_preamble() -> Result {
    stack.push(Word::from(0xbeef_u64))
}

#[instruction(EvmTypes = TestTypes)]
fn macro_concrete_eq(cx: _) -> out {
    *out = cx.state.host().block_env().number;
}

trait MacroTypesExt {}

impl MacroTypesExt for TestTypes {}

#[instruction(EvmTypes: MacroTypesExt)]
fn macro_type_bound(cx: _) {}

trait MacroHostExt {
    fn macro_bound_value(&self) -> Word;
}

impl MacroHostExt for TestHost {
    fn macro_bound_value(&self) -> Word {
        self.block.number
    }
}

#[instruction(EvmTypes = TestTypes)]
fn macro_assoc_bound(cx: _) -> out {
    *out = cx.state.host().macro_bound_value();
}

struct MacroConfig;

impl EvmConfig<TestTypes> for MacroConfig {
    const BASE_SPEC_ID: SpecId = SpecId::OSAKA;
    const OPCODE_CONFIG: &'static OpcodeConfig<TestTypes> = &macro_opcode_config();
}

const fn macro_opcode_config() -> OpcodeConfig<TestTypes> {
    let mut config = OpcodeConfig::<TestTypes>::base::<BaseEvmConfig<{ SpecId::OSAKA as u32 }>>();
    config.set_instruction::<macro_add<TestTypes>>(ADD_OPCODE, 0);
    config.set_instruction::<macro_dynamic_gas<TestTypes>>(DYNAMIC_GAS_OPCODE, 2);
    config.set_instruction::<macro_no_stack_preamble<TestTypes>>(NO_STACK_PREAMBLE_OPCODE, 0);
    config.set_instruction::<macro_concrete_eq>(CONCRETE_EQ_OPCODE, 0);
    config.set_instruction::<macro_type_bound<TestTypes>>(TYPE_BOUND_OPCODE, 0);
    config.set_instruction::<macro_assoc_bound>(ASSOC_BOUND_OPCODE, 0);
    config
}

fn run(config: RunConfig<'_>) -> TestInterpreter {
    let execution_config = ExecutionConfig::<TestTypes>::for_config::<MacroConfig>();
    let RunConfig { code, host, spec_id, tx_env, message, gas_limit, return_data } = config;
    let message = Message::<TestTypes> { code: legacy_bytecode(code), gas_limit, ..message };
    let mut inner = Interpreter::<TestTypes>::new(&tx_env, &message);
    *inner.return_data_mut() = return_data;
    let mut default_host = TestHost::default();
    let host = host.unwrap_or(&mut default_host);
    host.spec_id = spec_id;
    let err = inner.run(&execution_config, host);
    let (stack, stack_len, gas, memory, output) = inner.into_parts();
    TestInterpreter { stack, stack_len, gas, memory, output, err }
}

#[test]
fn instruction_macro_stack_inputs_and_output() {
    let mut code = Vec::new();
    push(&mut code, 2);
    push(&mut code, 3);
    code.extend([ADD_OPCODE, op::STOP]);

    let interp = run(RunConfig::new(code));

    assert_eq!(interp.err, InstrStop::Stop);
    assert_eq!(interp.stack(), [Word::from(5)]);
}

#[test]
fn instruction_macro_dynamic_gas_attribute() {
    let interp = run(RunConfig::new([DYNAMIC_GAS_OPCODE, op::STOP]));

    assert_eq!(interp.err, InstrStop::Stop);
    assert_eq!(interp.stack(), [Word::from(0xda_u64)]);
    assert_eq!(interp.gas_remaining(), 9_995);
}

#[test]
fn instruction_macro_no_stack_preamble_attribute() {
    let interp = run(RunConfig::new([NO_STACK_PREAMBLE_OPCODE, op::STOP]));

    assert_eq!(interp.err, InstrStop::Stop);
    assert_eq!(interp.stack(), [Word::from(0xbeef_u64)]);
}

#[test]
fn instruction_macro_concrete_evm_types_equals_attribute() {
    let mut host = TestHost {
        block: BlockEnvExt { number: Word::from(42), ..BlockEnvExt::default() },
        ..TestHost::default()
    };
    let interp = run(RunConfig::new([CONCRETE_EQ_OPCODE, op::STOP]).host(&mut host));

    assert_eq!(interp.err, InstrStop::Stop);
    assert_eq!(interp.stack(), [Word::from(42)]);
}

#[test]
fn instruction_macro_evm_types_colon_bound_attribute() {
    let mut host = TestHost {
        block: BlockEnvExt { number: Word::from(31337), ..BlockEnvExt::default() },
        ..TestHost::default()
    };
    let interp = run(RunConfig::new([TYPE_BOUND_OPCODE, op::STOP]).host(&mut host));

    assert_eq!(interp.err, InstrStop::Stop);
    assert!(interp.stack().is_empty());
}

#[test]
fn instruction_macro_evm_types_assoc_colon_bound_attribute() {
    let mut host = TestHost {
        block: BlockEnvExt { number: Word::from(31337), ..BlockEnvExt::default() },
        ..TestHost::default()
    };
    let interp = run(RunConfig::new([ASSOC_BOUND_OPCODE, op::STOP]).host(&mut host));

    assert_eq!(interp.err, InstrStop::Stop);
    assert_eq!(interp.stack(), [Word::from(31337)]);
}
