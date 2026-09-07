use std::{fmt, path::Path, sync::OnceLock};

use context_interface::{
    cfg::GasParams,
    context::{SStoreResult, SelfDestructResult, StateLoad},
    host::LoadError,
    journaled_state::AccountInfoLoad,
};
use revm_bytecode::opcode as op;
use revm_interpreter::{
    CallInput, Host, InputsImpl, Interpreter, SharedMemory,
    instructions::{gas_table_spec, instruction_table},
    interpreter::ExtBytecode,
};
use revm_primitives::{B256, HashMap, Log, hardfork::SpecId};
use similar_asserts_2_0::assert_eq;

use super::*;

/// Initializes `tracing_subscriber` for tests. Safe to call multiple times.
pub fn init_tracing() {
    let _ = tracing_subscriber::fmt::try_init();
}

/// Default test environment struct for test expected values.
#[derive(Clone, Debug)]
pub struct DefEnv {
    pub tx: DefTx,
    pub block: DefBlock,
    pub cfg: DefCfg,
}

#[derive(Clone, Debug)]
pub struct DefTx {
    pub caller: Address,
    pub blob_hashes: Vec<B256>,
}

#[derive(Clone, Debug)]
pub struct DefBlock {
    pub coinbase: Address,
    pub timestamp: U256,
    pub number: U256,
    pub difficulty: U256,
    pub prevrandao: Option<B256>,
    pub gas_limit: U256,
    pub basefee: U256,
}

impl DefBlock {
    pub fn get_blob_gasprice(&self) -> Option<u64> {
        Some(0) // Default blob gas price for tests
    }
}

#[derive(Clone, Debug)]
pub struct DefCfg {
    pub chain_id: u64,
}

impl DefEnv {
    pub fn effective_gas_price(&self) -> U256 {
        U256::from(0x4567)
    }
}

/// Returns the default test environment.
pub fn def_env() -> DefEnv {
    DefEnv {
        tx: DefTx {
            caller: Address::repeat_byte(0xcc),
            blob_hashes: vec![B256::repeat_byte(0x01), B256::repeat_byte(0x02)],
        },
        block: DefBlock {
            coinbase: Address::repeat_byte(0xcb),
            timestamp: U256::from(0x1234),
            number: DEF_BN,
            difficulty: U256::from(0xcdef),
            prevrandao: Some(B256::from(U256::from(0x0123))),
            gas_limit: U256::from(0x5678),
            basefee: U256::from(0x1231),
        },
        cfg: DefCfg { chain_id: 69 },
    }
}

/// Memory gas calculation: `num_words * 3 + num_words**2 / 512`.
pub fn memory_gas_cost(num_words: usize) -> u64 {
    (num_words as u64) * 3 + (num_words as u64) * (num_words as u64) / 512
}

pub struct TestCase<'a> {
    pub bytecode: &'a [u8],
    pub spec_id: SpecId,
    pub is_static: bool,
    pub gas_limit: u64,

    /// Override `inspect_stack` on the compiler. `None` uses the default (`true`).
    pub inspect_stack: Option<bool>,
    pub modify_ecx: Option<fn(&mut EvmContext<'_>)>,

    pub expected_return: InstructionResult,
    pub expected_stack: &'a [U256],
    pub expected_memory: &'a [u8],
    pub expected_gas: u64,
    pub expected_next_action: InterpreterAction,
    pub assert_host: Option<fn(&TestHost)>,
    pub assert_ecx: Option<fn(&EvmContext<'_>)>,
}

#[cfg(feature = "__fuzzing")]
impl<'a> arbitrary::Arbitrary<'a> for TestCase<'a> {
    fn arbitrary(u: &mut arbitrary::Unstructured<'a>) -> arbitrary::Result<Self> {
        let spec_id_range = 0..=(SpecId::OSAKA as u8 - 1);
        let spec_id = SpecId::try_from_u8(u.int_in_range(spec_id_range)?).unwrap_or(DEF_SPEC);

        let bytecode: &'a [u8] = u.arbitrary()?;

        Ok(Self::what_interpreter_says(bytecode, spec_id))
    }
}

impl Default for TestCase<'_> {
    fn default() -> Self {
        Self {
            bytecode: &[],
            spec_id: DEF_SPEC,
            is_static: false,
            gas_limit: DEF_GAS_LIMIT,
            inspect_stack: None,
            modify_ecx: None,
            expected_return: InstructionResult::Stop,
            expected_stack: &[],
            expected_memory: &[],
            expected_gas: 0,
            expected_next_action: ACTION_WHAT_INTERPRETER_SAYS,
            assert_host: None,
            assert_ecx: None,
        }
    }
}

impl fmt::Debug for TestCase<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TestCase")
            .field("bytecode", &format_bytecode(self.bytecode, self.spec_id))
            .field("spec_id", &self.spec_id)
            .field("inspect_stack", &self.inspect_stack)
            .field("modify_ecx", &self.modify_ecx.is_some())
            .field("expected_return", &self.expected_return)
            .field("expected_stack", &self.expected_stack)
            .field("expected_memory", &MemDisplay(self.expected_memory))
            .field("expected_gas", &self.expected_gas)
            .field("expected_next_action", &self.expected_next_action)
            .field("assert_host", &self.assert_host.is_some())
            .field("assert_ecx", &self.assert_ecx.is_some())
            .finish()
    }
}

impl<'a> TestCase<'a> {
    pub fn what_interpreter_says(bytecode: &'a [u8], spec_id: SpecId) -> Self {
        Self {
            bytecode,
            spec_id,
            is_static: false,
            gas_limit: DEF_GAS_LIMIT,
            inspect_stack: None,
            modify_ecx: None,
            expected_return: RETURN_WHAT_INTERPRETER_SAYS,
            expected_stack: STACK_WHAT_INTERPRETER_SAYS,
            expected_memory: MEMORY_WHAT_INTERPRETER_SAYS,
            expected_gas: GAS_WHAT_INTERPRETER_SAYS,
            expected_next_action: ACTION_WHAT_INTERPRETER_SAYS,
            assert_host: None,
            assert_ecx: None,
        }
    }
}

// Default values.
pub const DEF_SPEC: SpecId = SpecId::CANCUN;
pub static DEF_OPINFOS: std::sync::LazyLock<&'static [OpcodeInfo; 256]> =
    std::sync::LazyLock::new(|| op_info_map(DEF_SPEC));

pub const DEF_GAS_LIMIT: u64 = 100_000;
pub const DEF_GAS_LIMIT_U256: U256 = U256::from_le_slice(&DEF_GAS_LIMIT.to_le_bytes());

/// Default code address.
pub const DEF_ADDR: Address = Address::repeat_byte(0xba);
pub const DEF_CALLER: Address = Address::repeat_byte(0xca);
pub static DEF_CD: &[u8] = &[0xaa; 64];
pub static DEF_RD: &[u8] = &[0xbb; 64];
pub static DEF_DATA: &[u8] = &[0xcc; 64];
pub const DEF_VALUE: U256 = uint!(123_456_789_U256);
pub static DEF_STORAGE: OnceLock<HashMap<U256, U256>> = OnceLock::new();
pub static DEF_CODEMAP: OnceLock<HashMap<Address, revm_bytecode::Bytecode>> = OnceLock::new();
pub const OTHER_ADDR: Address = Address::repeat_byte(0x69);
pub const DEF_BN: U256 = uint!(500_U256);

pub const RETURN_WHAT_INTERPRETER_SAYS: InstructionResult = InstructionResult::PrecompileError;
pub const STACK_WHAT_INTERPRETER_SAYS: &[U256] =
    &[U256::from_be_slice(&GAS_WHAT_INTERPRETER_SAYS.to_be_bytes())];
pub const MEMORY_WHAT_INTERPRETER_SAYS: &[u8] = &GAS_WHAT_INTERPRETER_SAYS.to_be_bytes();
pub const GAS_WHAT_INTERPRETER_SAYS: u64 = 0x4682e332d6612de1;
pub const ACTION_WHAT_INTERPRETER_SAYS: InterpreterAction =
    InterpreterAction::Return(InterpreterResult {
        gas: Gas::new(GAS_WHAT_INTERPRETER_SAYS),
        output: Bytes::from_static(MEMORY_WHAT_INTERPRETER_SAYS),
        result: RETURN_WHAT_INTERPRETER_SAYS,
    });

pub fn def_storage() -> &'static HashMap<U256, U256> {
    DEF_STORAGE.get_or_init(|| {
        let mut map = HashMap::default();
        map.insert(U256::from(0), U256::from(1));
        map.insert(U256::from(1), U256::from(2));
        map.insert(U256::from(69), U256::from(42));
        map
    })
}

pub fn def_codemap() -> &'static HashMap<Address, revm_bytecode::Bytecode> {
    DEF_CODEMAP.get_or_init(|| {
        let mut map = HashMap::default();
        map.insert(
            OTHER_ADDR,
            revm_bytecode::Bytecode::new_raw(Bytes::from_static(&[
                op::PUSH1,
                0x69,
                op::PUSH1,
                0x42,
                op::ADD,
                op::STOP,
            ])),
        );
        map
    })
}

/// Test host that implements [`Host`] trait for testing.
pub struct TestHost {
    pub storage: HashMap<U256, U256>,
    pub transient_storage: HashMap<U256, U256>,
    pub code_map: &'static HashMap<Address, revm_bytecode::Bytecode>,
    pub selfdestructs: Vec<(Address, Address)>,
    pub logs: Vec<Log>,
    pub gas_params: GasParams,
}

impl Default for TestHost {
    fn default() -> Self {
        Self::new()
    }
}

impl TestHost {
    pub fn new() -> Self {
        Self::with_spec(DEF_SPEC)
    }

    pub fn with_spec(spec_id: SpecId) -> Self {
        Self {
            storage: def_storage().clone(),
            transient_storage: HashMap::default(),
            code_map: def_codemap(),
            selfdestructs: Vec::new(),
            logs: Vec::new(),
            gas_params: GasParams::new_spec(spec_id),
        }
    }
}

impl Host for TestHost {
    fn basefee(&self) -> U256 {
        U256::from(0x1231)
    }

    fn blob_gasprice(&self) -> U256 {
        U256::ZERO
    }

    fn gas_limit(&self) -> U256 {
        U256::from(0x5678)
    }

    fn gas_params(&self) -> &GasParams {
        &self.gas_params
    }

    fn is_amsterdam_eip8037_enabled(&self) -> bool {
        false
    }

    fn difficulty(&self) -> U256 {
        U256::from(0xcdef)
    }

    fn prevrandao(&self) -> Option<U256> {
        Some(U256::from(0x0123))
    }

    fn block_number(&self) -> U256 {
        DEF_BN
    }

    fn timestamp(&self) -> U256 {
        U256::from(0x1234)
    }

    fn beneficiary(&self) -> Address {
        Address::repeat_byte(0xcb)
    }

    fn slot_num(&self) -> U256 {
        U256::ZERO
    }

    fn chain_id(&self) -> U256 {
        U256::from(69)
    }

    fn effective_gas_price(&self) -> U256 {
        U256::from(0x4567)
    }

    fn caller(&self) -> Address {
        Address::repeat_byte(0xcc)
    }

    fn blob_hash(&self, number: usize) -> Option<U256> {
        let env = def_env();
        env.tx.blob_hashes.get(number).map(|h| (*h).into())
    }

    fn max_initcode_size(&self) -> usize {
        // EIP-3860: Max initcode size is 2 * MAX_CODE_SIZE = 2 * 24576 = 49152
        49152
    }

    fn block_hash(&mut self, number: u64) -> Option<B256> {
        Some(U256::from(number).into())
    }

    fn selfdestruct(
        &mut self,
        address: Address,
        target: Address,
        _skip_cold_load: bool,
    ) -> Result<StateLoad<SelfDestructResult>, LoadError> {
        self.selfdestructs.push((address, target));

        Ok(StateLoad::new(
            SelfDestructResult {
                had_value: false,
                target_exists: true,
                previously_destroyed: false,
            },
            false,
        ))
    }

    fn log(&mut self, log: Log) {
        self.logs.push(log);
    }

    fn tstore(&mut self, _address: Address, key: U256, value: U256) {
        self.transient_storage.insert(key, value);
    }

    fn tload(&mut self, _address: Address, key: U256) -> U256 {
        self.transient_storage.get(&key).copied().unwrap_or(U256::ZERO)
    }

    fn load_account_info_skip_cold_load(
        &mut self,
        address: Address,
        load_code: bool,
        _skip_cold_load: bool,
    ) -> Result<AccountInfoLoad<'_>, LoadError> {
        use std::borrow::Cow;

        use revm_state::AccountInfo;

        let code = if load_code {
            // Return actual code if found, otherwise empty bytecode
            Some(self.code_map.get(&address).cloned().unwrap_or_default())
        } else {
            None
        };

        // Return address byte as balance (test convention)
        // The balance is the last byte of the address
        let balance = U256::from(address.0[19]);

        // Calculate code hash from the actual bytecode
        let code_hash = if let Some(bytecode) = self.code_map.get(&address) {
            keccak256(bytecode.original_byte_slice())
        } else {
            KECCAK_EMPTY
        };

        // Create owned account info
        let info = AccountInfo { balance, nonce: 0, code_hash, account_id: None, code };

        let is_empty = info.code.is_none() && info.balance.is_zero() && info.nonce == 0;

        Ok(AccountInfoLoad { account: Cow::Owned(info), is_cold: false, is_empty })
    }

    fn sstore_skip_cold_load(
        &mut self,
        _address: Address,
        key: U256,
        value: U256,
        _skip_cold_load: bool,
    ) -> Result<StateLoad<SStoreResult>, LoadError> {
        let original = self.storage.get(&key).copied().unwrap_or(U256::ZERO);
        self.storage.insert(key, value);
        Ok(StateLoad::new(
            SStoreResult { original_value: original, present_value: original, new_value: value },
            false,
        ))
    }

    fn sload_skip_cold_load(
        &mut self,
        _address: Address,
        key: U256,
        _skip_cold_load: bool,
    ) -> Result<StateLoad<U256>, LoadError> {
        let value = self.storage.get(&key).copied().unwrap_or(U256::ZERO);
        Ok(StateLoad::new(value, false))
    }
}

pub fn with_evm_context<F: FnOnce(&mut EvmContext<'_>, &mut EvmStack, &mut usize) -> R, R>(
    bytecode: &[u8],
    spec_id: SpecId,
    f: F,
) -> R {
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
        Interpreter::new(SharedMemory::new(), ext_bytecode, input, false, spec_id, DEF_GAS_LIMIT);

    let mut host = TestHost::with_spec(spec_id);

    let (mut ecx, stack, stack_len) =
        EvmContext::from_interpreter_with_stack(&mut interpreter, &mut host);
    f(&mut ecx, stack, stack_len)
}

pub fn set_test_dump<B: Backend>(compiler: &mut EvmCompiler<B>, module_path: &str) {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap().parent().unwrap();
    let mut dump_path = root.to_path_buf();
    dump_path.push("target");
    dump_path.push("tests_dump");
    // Skip `revmc::tests`.
    dump_path.extend(module_path.split("::").skip(2));
    dump_path.push(format!("{:?}", compiler.opt_level()));
    compiler.set_dump_to(Some(dump_path));
}

pub fn run_test_case<B: Backend>(test_case: &TestCase<'_>, compiler: &mut EvmCompiler<B>) {
    let TestCase { bytecode, spec_id, .. } = *test_case;
    compiler.inspect_stack(test_case.inspect_stack.unwrap_or(true));
    // compiler.debug_assertions(false);
    let f = unsafe { compiler.jit("test", bytecode, spec_id) }.unwrap();
    run_compiled_test_case(test_case, f);
}

fn run_compiled_test_case(test_case: &TestCase<'_>, f: EvmCompilerFn) {
    let TestCase {
        bytecode,
        spec_id,
        is_static,
        gas_limit,
        inspect_stack: _,
        modify_ecx,
        expected_return,
        expected_stack,
        expected_memory,
        expected_gas,
        ref expected_next_action,
        assert_host,
        assert_ecx,
    } = *test_case;

    with_evm_context(bytecode, spec_id, |ecx, stack, stack_len| {
        if is_static {
            ecx.is_static = true;
        }
        if gas_limit != DEF_GAS_LIMIT {
            ecx.gas = Gas::new(gas_limit);
        }
        if let Some(modify_ecx) = modify_ecx {
            modify_ecx(ecx);
            ecx.refresh_memory_cache();
        }

        // Interpreter - run via instruction table
        let input = InputsImpl {
            target_address: DEF_ADDR,
            bytecode_address: None,
            caller_address: DEF_CALLER,
            input: CallInput::Bytes(Bytes::from_static(DEF_CD)),
            call_value: DEF_VALUE,
        };
        let bytecode_obj = revm_bytecode::Bytecode::new_raw(Bytes::copy_from_slice(bytecode));
        let ext_bytecode = ExtBytecode::new(bytecode_obj);
        let mut interpreter = Interpreter::new(
            SharedMemory::new(),
            ext_bytecode,
            input,
            is_static,
            spec_id,
            gas_limit,
        );

        let table = instruction_table::<revm_interpreter::interpreter::EthInterpreter, TestHost>();
        let gas_table = gas_table_spec(spec_id);
        let mut int_host = TestHost::with_spec(spec_id);
        let interpreter_action = interpreter.run_plain(&table, &gas_table, &mut int_host);

        let int_result = match &interpreter_action {
            InterpreterAction::Return(result) => result.result,
            _ => InstructionResult::Stop,
        };

        let mut expected_return = expected_return;
        if expected_return == RETURN_WHAT_INTERPRETER_SAYS {
            expected_return = int_result;
        } else if modify_ecx.is_none() {
            // Only check interpreter return if modify_ecx is not set.
            // When modify_ecx is used, it only modifies the JIT context, not the interpreter's
            // input, so the interpreter may return a different result.
            assert_eq!(int_result, expected_return, "interpreter return value mismatch");
        }

        // When modify_ecx is set, the interpreter runs with different inputs than the JIT,
        // so we cannot use interpreter results as expected values or compare against them.
        let skip_interpreter_checks = modify_ecx.is_some() || expected_return.is_halt();

        let mut expected_stack = expected_stack;
        if expected_stack == STACK_WHAT_INTERPRETER_SAYS {
            if skip_interpreter_checks {
                expected_stack = &[]; // Will skip comparison below
            } else {
                expected_stack = interpreter.stack.data();
            }
        } else if !skip_interpreter_checks {
            assert_eq!(interpreter.stack.data(), expected_stack, "interpreter stack mismatch");
        }

        let interpreter_memory = interpreter.memory.context_memory();
        let mut expected_memory = expected_memory;
        if expected_memory == MEMORY_WHAT_INTERPRETER_SAYS {
            if skip_interpreter_checks {
                expected_memory = &[]; // Will skip comparison below
            } else {
                expected_memory = &*interpreter_memory;
            }
        } else if !skip_interpreter_checks {
            assert_eq!(
                MemDisplay(&interpreter_memory),
                MemDisplay(expected_memory),
                "interpreter memory mismatch"
            );
        }

        let mut expected_gas = expected_gas;
        if expected_gas == GAS_WHAT_INTERPRETER_SAYS {
            if skip_interpreter_checks {
                expected_gas = 0; // Will skip comparison below
            } else {
                expected_gas = interpreter.gas.total_gas_spent();
            }
        } else if !skip_interpreter_checks {
            assert_eq!(interpreter.gas.total_gas_spent(), expected_gas, "interpreter gas mismatch");
        }

        // Track whether we should skip JIT stack/gas/memory comparisons
        let skip_jit_stack =
            skip_interpreter_checks && test_case.expected_stack == STACK_WHAT_INTERPRETER_SAYS;
        let skip_jit_memory =
            skip_interpreter_checks && test_case.expected_memory == MEMORY_WHAT_INTERPRETER_SAYS;
        let skip_jit_gas =
            skip_interpreter_checks && test_case.expected_gas == GAS_WHAT_INTERPRETER_SAYS;

        // This is what the interpreter returns when the internal action is None in `run`.
        let default_action = InterpreterAction::Return(InterpreterResult {
            result: int_result,
            output: Bytes::new(),
            gas: interpreter.gas,
        });
        let mut expected_next_action = expected_next_action;
        if *expected_next_action == ACTION_WHAT_INTERPRETER_SAYS {
            expected_next_action = &interpreter_action;
        } else if modify_ecx.is_none() {
            // Only check interpreter action if modify_ecx is not set.
            // When modify_ecx is used, it only modifies the JIT context, not the interpreter's
            // input, so the interpreter may return a different action.
            assert_actions(&interpreter_action, expected_next_action);
        }

        if let Some(assert_host) = assert_host {
            assert_host(&int_host);
        }

        let actual_return = unsafe { f.call(stack, stack_len, ecx) };

        if matches!(
            actual_return,
            // We can have a stack overflow/underflow before other error codes due to sections.
            |InstructionResult::StackOverflow| InstructionResult::StackUnderflow
            // Any OOG is equivalent. We skip `InvalidOperand` sometimes.
            | InstructionResult::OutOfGas | InstructionResult::MemoryOOG | InstructionResult::InvalidOperandOOG
        ) {
            assert_eq!(
                actual_return.is_halt(),
                expected_return.is_halt(),
                "return value mismatch: {actual_return:?} != {expected_return:?}"
            );
        } else {
            assert_eq!(actual_return, expected_return, "return value mismatch");
        }

        let actual_stack =
            unsafe { stack.as_slice(*stack_len).iter().map(|x| x.to_u256()).collect::<Vec<_>>() };

        // On EVM halt all available gas is consumed, so resulting stack, memory, and gas do not
        // matter. We do less work than the interpreter by bailing out earlier due to sections.
        if !actual_return.is_halt() {
            if !skip_jit_stack {
                assert_eq!(actual_stack, *expected_stack, "stack mismatch");
            }

            if !skip_jit_memory {
                assert_eq!(
                    MemDisplay(&ecx.memory.context_memory()),
                    MemDisplay(expected_memory),
                    "memory mismatch"
                );
            }

            if !skip_jit_gas {
                assert_eq!(ecx.gas.total_gas_spent(), expected_gas, "gas mismatch");
            }
        }

        let actual_next_action = match ecx.next_action.as_ref() {
            Some(action) => action,
            None => &default_action,
        };
        assert_actions(actual_next_action, expected_next_action);

        if let Some(_assert_host) = assert_host {
            let host = unsafe { &*(ecx.host as *mut dyn Host as *mut TestHost) };
            _assert_host(host);
        }

        if let Some(assert_ecx) = assert_ecx {
            assert_ecx(ecx);
        }
    });
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct MemDisplay<'a>(&'a [u8]);
impl fmt::Debug for MemDisplay<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let chunks = self.0.chunks(32).map(revm_primitives::hex::encode_prefixed);
        f.debug_list().entries(chunks).finish()
    }
}

#[track_caller]
fn assert_actions(actual: &InterpreterAction, expected: &InterpreterAction) {
    match (actual, expected) {
        (InterpreterAction::Return(result), InterpreterAction::Return(expected_result)) => {
            assert_eq!(result.result, expected_result.result, "result mismatch");
            assert_eq!(result.output, expected_result.output, "result output mismatch");
            if expected_result.gas.limit() != GAS_WHAT_INTERPRETER_SAYS {
                assert_eq!(
                    result.gas.total_gas_spent(),
                    expected_result.gas.total_gas_spent(),
                    "result gas mismatch"
                );
            }
        }
        (
            InterpreterAction::NewFrame(FrameInput::Call(actual_call)),
            InterpreterAction::NewFrame(FrameInput::Call(expected_call)),
        ) => {
            // Compare CallInputs fields, allowing for differences in input representation
            // and known_bytecode (JIT doesn't preload, interpreter may)
            assert_eq!(
                actual_call.return_memory_offset, expected_call.return_memory_offset,
                "return_memory_offset mismatch"
            );
            assert_eq!(actual_call.gas_limit, expected_call.gas_limit, "gas_limit mismatch");
            assert_eq!(
                actual_call.bytecode_address, expected_call.bytecode_address,
                "bytecode_address mismatch"
            );
            assert_eq!(
                actual_call.target_address, expected_call.target_address,
                "target_address mismatch"
            );
            assert_eq!(actual_call.caller, expected_call.caller, "caller mismatch");
            assert_eq!(actual_call.value, expected_call.value, "value mismatch");
            assert_eq!(actual_call.scheme, expected_call.scheme, "scheme mismatch");
            assert_eq!(actual_call.is_static, expected_call.is_static, "is_static mismatch");
            // Note: We don't compare `input` directly as it may use different shared-memory
            // ranges.
            // Note: We don't compare `known_bytecode` as JIT doesn't preload.
        }
        (
            InterpreterAction::NewFrame(FrameInput::Create(actual_create)),
            InterpreterAction::NewFrame(FrameInput::Create(expected_create)),
        ) => {
            // Compare CreateInputs fields
            assert_eq!(actual_create.caller(), expected_create.caller(), "caller mismatch");
            assert_eq!(actual_create.scheme(), expected_create.scheme(), "scheme mismatch");
            assert_eq!(actual_create.value(), expected_create.value(), "value mismatch");
            assert_eq!(
                actual_create.init_code(),
                expected_create.init_code(),
                "init_code mismatch"
            );
            assert_eq!(
                actual_create.gas_limit(),
                expected_create.gas_limit(),
                "gas_limit mismatch"
            );
        }
        (a, b) => assert_eq!(a, b, "next action mismatch"),
    }
}

/// Insert a call outcome into the interpreter state (for testing call_with_interpreter).
///
/// Mimics revm-handler's insert_call_outcome: pushes success indicator, copies return data
/// to memory, and returns unspent gas.
pub fn insert_call_outcome_test(
    interpreter: &mut revm_interpreter::Interpreter,
    outcome: InterpreterResult,
    return_memory_offset: Option<std::ops::Range<usize>>,
) {
    use revm_interpreter::interpreter_types::ReturnData;

    let ins_result = outcome.result;
    let out_gas = outcome.gas;
    let returned_len = outcome.output.len();

    interpreter.return_data.set_buffer(outcome.output);

    let success_indicator = if ins_result.is_ok() { U256::from(1) } else { U256::ZERO };
    let _ = interpreter.stack.push(success_indicator);

    if ins_result.is_ok_or_revert() {
        interpreter.gas.erase_cost(out_gas.remaining());

        if let Some(mem_range) = return_memory_offset {
            let target_len = std::cmp::min(mem_range.len(), returned_len);
            if target_len > 0 {
                interpreter
                    .memory
                    .set(mem_range.start, &interpreter.return_data.buffer()[..target_len]);
            }
        }
    }

    if ins_result.is_ok() {
        interpreter.gas.record_refund(out_gas.refunded());
    }
}
