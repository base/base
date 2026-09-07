use alloc::{boxed::Box, vec::Vec};
use core::{assert_matches, ops::Range};

use alloy_primitives::{Address, B256, Bytes, Log};

use crate::{
    BaseEvmConfigSelector, EvmFeatures, EvmTypesHost, ExecutionConfig, SpecId,
    bytecode::Bytecode,
    constants::CALL_DEPTH_LIMIT,
    env::{BlockEnv, BlockEnvExt, TxEnv, TxEnvExt},
    evm::{AccountLoad, SLoad, SStore, SelfDestructResult},
    interpreter::{
        Gas, GasTracker, Host, InstrStop, Interpreter, Memory, Message, MessageExt, MessageKind,
        MessageResult, MessageResultExt, StackBacking, Word, op,
    },
    storage_key::{StorageKey, StorageKeyMap},
};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) struct TestTypes;

impl EvmTypesHost for TestTypes {
    type ConfigSelector = crate::BaseEvmConfigSelector;
    type SpecId = crate::SpecId;
    type Tx = ();
    type EvmExt = ();
    type MessageExt = ();
    type MessageResultExt = ();
    type TxEnvExt = ();
    type TxResultExt = ();
    type BlockEnvExt = ();
    type Host<'a> = TestHost;
}

#[derive(Debug)]
pub(crate) struct TestHost {
    pub(crate) spec_id: SpecId,
    pub(crate) block: BlockEnv<TestTypes>,
    pub(crate) code_hash: B256,
    pub(crate) code: Bytes,
    pub(crate) exists: bool,
    pub(crate) is_empty: bool,
    pub(crate) is_cold: bool,
    pub(crate) is_touched: bool,
    pub(crate) missing_block_hash: bool,
    pub(crate) storage: StorageKeyMap<Word>,
    pub(crate) original_storage: StorageKeyMap<Word>,
    pub(crate) transient_storage: StorageKeyMap<Word>,
    pub(crate) logs: Vec<Log>,
    pub(crate) execute_result: MessageResult<TestTypes>,
    pub(crate) selfdestruct_result: SelfDestructResult,
    pub(crate) selfdestruct_error: Option<InstrStop>,
    pub(crate) calls: Vec<Message<TestTypes>>,
    pub(crate) call_static_flags: Vec<bool>,
    pub(crate) selfdestructs: Vec<(Address, Address, bool)>,
    pub(crate) new_account_checks: Vec<Address>,
}

impl Default for TestHost {
    fn default() -> Self {
        Self {
            spec_id: SpecId::OSAKA,
            block: BlockEnvExt::default(),
            code_hash: B256::ZERO,
            code: Bytes::new(),
            exists: true,
            is_empty: false,
            is_cold: false,
            is_touched: false,
            missing_block_hash: false,
            storage: StorageKeyMap::default(),
            original_storage: StorageKeyMap::default(),
            transient_storage: StorageKeyMap::default(),
            logs: Vec::new(),
            execute_result: MessageResultExt {
                stop: InstrStop::Return,
                ..MessageResultExt::default()
            },
            selfdestruct_result: SelfDestructResult::default(),
            selfdestruct_error: None,
            calls: Vec::new(),
            call_static_flags: Vec::new(),
            selfdestructs: Vec::new(),
            new_account_checks: Vec::new(),
        }
    }
}

impl Host<TestTypes> for TestHost {
    fn spec_id(&self) -> SpecId {
        self.spec_id
    }

    fn block_env(&mut self) -> &BlockEnv<TestTypes> {
        &self.block
    }

    fn load_account(
        &mut self,
        address: &Address,
        load_code: bool,
        skip_cold_load: bool,
    ) -> Result<AccountLoad, InstrStop> {
        if skip_cold_load && self.is_cold {
            return Err(InstrStop::OutOfGas);
        }
        Ok(AccountLoad {
            balance: address.into_word().into(),
            nonce: 0,
            code_hash: self.code_hash,
            code: if load_code {
                Bytecode::new_legacy(self.code.clone())
            } else {
                Bytecode::default()
            },
            exists: self.exists,
            is_empty: self.is_empty,
            is_cold: self.is_cold,
            _non_exhaustive: (),
        })
    }

    fn target_is_empty_for_new_account_gas(
        &mut self,
        address: &Address,
        features: EvmFeatures,
    ) -> Result<bool, InstrStop> {
        self.new_account_checks.push(*address);
        if features.contains(EvmFeatures::EIP161) {
            return Ok(!self.exists || self.is_empty);
        }
        Ok(!self.exists && !self.is_touched)
    }

    fn block_hash(&mut self, number: &Word) -> Result<B256, InstrStop> {
        if self.missing_block_hash {
            return Err(InstrStop::FatalExternalError);
        }
        Ok(B256::with_last_byte(number.wrapping_to::<u8>()))
    }

    fn sload(
        &mut self,
        address: &Address,
        key: &Word,
        skip_cold_load: bool,
    ) -> Result<SLoad, InstrStop> {
        if skip_cold_load && self.is_cold {
            return Err(InstrStop::OutOfGas);
        }
        Ok(SLoad {
            value: self.storage.get(&StorageKey::new(*address, *key)).copied().unwrap_or_default(),
            is_cold: self.is_cold,
            _non_exhaustive: (),
        })
    }

    fn sstore(
        &mut self,
        address: &Address,
        key: &Word,
        value: &Word,
        skip_cold_load: bool,
    ) -> Result<SStore, InstrStop> {
        if skip_cold_load && self.is_cold {
            return Err(InstrStop::OutOfGas);
        }
        let storage_key = StorageKey::new(*address, *key);
        let present_value = self.storage.get(&storage_key).copied().unwrap_or_default();
        let original_value = *self.original_storage.entry(storage_key).or_insert(present_value);
        if value.is_zero() {
            self.storage.remove(&storage_key);
        } else {
            self.storage.insert(storage_key, *value);
        }
        Ok(SStore {
            original_value,
            present_value,
            new_value: *value,
            is_cold: self.is_cold,
            _non_exhaustive: (),
        })
    }

    fn tload(&mut self, address: &Address, key: &Word) -> Word {
        self.transient_storage.get(&StorageKey::new(*address, *key)).copied().unwrap_or_default()
    }

    fn tstore(&mut self, address: &Address, key: &Word, value: &Word) {
        self.transient_storage.insert(StorageKey::new(*address, *key), *value);
    }

    fn log(&mut self, log: Log) {
        self.logs.push(log);
    }

    fn execute_message(
        &mut self,
        _tx_env: &TxEnv<TestTypes>,
        message: &mut Message<TestTypes>,
    ) -> MessageResult<TestTypes> {
        // Mimics the depth limit enforced by the real host.
        if message.depth > CALL_DEPTH_LIMIT {
            return MessageResultExt {
                stop: InstrStop::CallTooDeep,
                gas: GasTracker::new(message.gas_limit),
                ..Default::default()
            };
        }
        self.call_static_flags
            .push(message.caller_is_static || message.kind == MessageKind::StaticCall);
        self.calls.push(message.clone());
        self.execute_result.clone()
    }

    fn selfdestruct(
        &mut self,
        contract: &Address,
        target: &Address,
        skip_cold_load: bool,
    ) -> Result<SelfDestructResult, InstrStop> {
        if let Some(err) = self.selfdestruct_error {
            return Err(err);
        }
        self.selfdestructs.push((*contract, *target, skip_cold_load));
        Ok(self.selfdestruct_result)
    }
}

pub(crate) struct TestInterpreter {
    pub(crate) stack: Box<StackBacking>,
    pub(crate) stack_len: usize,
    pub(crate) gas: Gas,
    pub(crate) memory: Memory,
    pub(crate) output: Range<u32>,
    pub(crate) err: InstrStop,
}

impl TestInterpreter {
    pub(crate) fn stack(&self) -> &[Word] {
        unsafe { core::slice::from_raw_parts(self.stack.as_ptr().cast(), self.stack_len) }
    }

    pub(crate) fn memory(&mut self, offset: usize, len: usize) -> &[u8] {
        self.memory.slice(offset, len)
    }

    pub(crate) fn gas_remaining(&self) -> u64 {
        self.gas.remaining()
    }

    pub(crate) fn gas_refunded(&self) -> i64 {
        self.gas.refunded()
    }

    pub(crate) fn state_gas_spent(&self) -> i64 {
        self.gas.state_gas_spent()
    }

    pub(crate) fn output(&self) -> &[u8] {
        self.memory.slice(self.output.start as usize, self.output.len())
    }
}

pub(crate) struct RunConfig<'a> {
    pub(crate) code: Vec<u8>,
    pub(crate) host: Option<&'a mut TestHost>,
    pub(crate) spec_id: SpecId,
    pub(crate) tx_env: TxEnv<TestTypes>,
    pub(crate) message: Message<TestTypes>,
    pub(crate) gas_limit: u64,
    pub(crate) return_data: Bytes,
}

impl<'a> RunConfig<'a> {
    pub(crate) fn new(code: impl Into<Vec<u8>>) -> Self {
        Self { code: code.into(), ..Self::default() }
    }

    pub(crate) fn host(mut self, host: &'a mut TestHost) -> Self {
        self.host = Some(host);
        self
    }

    pub(crate) const fn spec(mut self, spec_id: SpecId) -> Self {
        self.spec_id = spec_id;
        self
    }

    pub(crate) fn tx_env(mut self, tx_env: TxEnv<TestTypes>) -> Self {
        self.tx_env = tx_env;
        self
    }

    pub(crate) fn message(mut self, message: Message<TestTypes>) -> Self {
        self.message = message;
        self
    }

    pub(crate) fn staticcall(mut self) -> Self {
        self.message.kind = MessageKind::StaticCall;
        self
    }

    pub(crate) const fn gas_limit(mut self, gas_limit: u64) -> Self {
        self.gas_limit = gas_limit;
        self
    }

    pub(crate) fn return_data(mut self, return_data: Bytes) -> Self {
        self.return_data = return_data;
        self
    }
}

impl Default for RunConfig<'_> {
    fn default() -> Self {
        Self {
            code: Vec::new(),
            host: None,
            spec_id: SpecId::OSAKA,
            tx_env: TxEnvExt::default(),
            message: MessageExt { gas_limit: 10_000, ..MessageExt::default() },
            gas_limit: 10_000,
            return_data: Bytes::new(),
        }
    }
}

pub(crate) fn run(config: RunConfig<'_>) -> TestInterpreter {
    let RunConfig { code, host, spec_id, tx_env, message, gas_limit, return_data } = config;
    let message = Message::<TestTypes> { code: legacy_bytecode(code), gas_limit, ..message };
    let mut inner = Interpreter::<TestTypes>::new(&tx_env, &message);
    *inner.return_data_mut() = return_data;
    let mut default_host = TestHost::default();
    let host = host.unwrap_or(&mut default_host);
    host.spec_id = spec_id;
    let config = ExecutionConfig::for_base_spec::<BaseEvmConfigSelector>(spec_id);
    let err = inner.run(&config, host);
    let (stack, stack_len, gas, memory, output) = inner.into_parts();
    TestInterpreter { stack, stack_len, gas, memory, output, err }
}

pub(crate) trait ToWord {
    fn to_word(self) -> Word;
}

impl ToWord for Word {
    fn to_word(self) -> Word {
        self
    }
}

impl ToWord for i32 {
    fn to_word(self) -> Word {
        Word::from(self as u64)
    }
}

impl ToWord for u64 {
    fn to_word(self) -> Word {
        Word::from(self)
    }
}

impl ToWord for usize {
    fn to_word(self) -> Word {
        Word::from(self)
    }
}

pub(crate) fn stack_code<T: ToWord, const N: usize>(inputs: [T; N], opcode: u8) -> Vec<u8> {
    let mut code = Vec::new();
    for input in inputs.into_iter().rev() {
        push(&mut code, input);
    }
    code.extend([opcode, op::STOP]);
    code
}

pub(crate) fn run_stack<T: ToWord, const N: usize>(inputs: [T; N], opcode: u8) -> TestInterpreter {
    run(RunConfig::new(stack_code(inputs, opcode)))
}

pub(crate) fn assert_stack_words(inputs: &[Word], opcode: u8, expected: &[Word]) {
    let mut code = Vec::new();
    for input in inputs.iter().rev() {
        push(&mut code, *input);
    }
    code.extend([opcode, op::STOP]);
    let interp = run(RunConfig::new(code));
    assert_matches!(interp.err, InstrStop::Stop);
    assert_eq!(interp.stack(), expected);
}

macro_rules! assert_stack {
    ($op:ident($($input:expr),* $(,)?), $expected:expr $(,)?) => {{
        let inputs = [$($crate::interpreter::Word::from($input)),*];
        let expected = [$crate::interpreter::Word::from($expected)];
        $crate::test_utils::assert_stack_words(
            &inputs,
            $crate::interpreter::op::$op,
            &expected,
        );
    }};
}
pub(crate) use assert_stack;

pub(crate) fn push(code: &mut Vec<u8>, value: impl ToWord) {
    let value = value.to_word();
    if value.is_zero() {
        code.extend([op::PUSH1, 0]);
        return;
    }

    let bytes = value.to_be_bytes::<32>();
    let start = bytes.iter().position(|&byte| byte != 0).unwrap();
    let len = bytes.len() - start;
    code.push(op::PUSH1 + len as u8 - 1);
    code.extend_from_slice(&bytes[start..]);
}

pub(crate) fn push_all<T: ToWord, const N: usize>(code: &mut Vec<u8>, values: [T; N]) {
    for value in values {
        push(code, value);
    }
}

pub(crate) fn push_address(code: &mut Vec<u8>, address: &Address) {
    push(code, Word::from_be_slice(address.as_slice()));
}

pub(crate) fn neg(value: u64) -> Word {
    Word::ZERO.wrapping_sub(Word::from(value))
}

pub(crate) fn legacy_bytecode(code: impl Into<Vec<u8>>) -> Bytecode {
    Bytecode::new_legacy(Bytes::from(code.into()))
}
