//! Semantic coverage of a block's execution, gathered by a revm [`Inspector`] for
//! coverage-guided fuzzing.

use std::collections::{BTreeMap, HashMap, HashSet};

use alloy_primitives::{Address, U256};
use revm::{
    bytecode::opcode::OpCode,
    context::{ContextTr, CreateScheme},
    inspector::Inspector,
    interpreter::{
        CallInputs, CallOutcome, CallScheme, CreateInputs, CreateOutcome, InstructionResult,
        Interpreter, interpreter::EthInterpreter, interpreter_types::Jumps,
    },
};

/// Collects coverage features while a block executes.
///
/// Features are named strings mapped to hit counts. Per-instruction work is two array
/// increments; everything else is recorded per call frame, per transaction, or per block:
///
/// - `op:<fork>:<opcode>:d<depth>`: opcode executed at a call depth bucket.
/// - `gas:<opcode>:<cost>`: gas an instruction charged (exact up to 32, then a power of two), which
///   separates cold and warm accesses, `SSTORE` cases, memory expansion, and forwarded call gas.
/// - `end:<opcode>:<result>`: the last opcode a frame ran and how the frame ended (halt reason).
/// - `call:<scheme>:<result>:d<depth>[:value][:nocode]`: call outcomes.
/// - `create:<scheme>:<result>:d<depth>:init<len>`: CREATE and CREATE2 outcomes.
/// - `pc:<address>:len<len>:<result>` and `pcgas:<address>:<gas>`: standard precompiles.
/// - `np:<precompile>:<selector>:<result>[:<error>]`: native precompiles (unknown selectors
///   collapse to `*`).
/// - `sd:<beneficiary>:<value>:<age>`: SELFDESTRUCT kinds.
/// - `top:t<type>:<result>`, `refund:<kind>`, `maxdepth:<depth>`: per transaction.
///
/// The adapter adds transaction-level features (`tx:` and `reject:`) after the block.
#[derive(Debug, Default)]
pub struct Coverage {
    /// Fork label of `op:` features.
    pub fork: String,
    /// Opcode by depth bucket counters.
    pub ops: Vec<u32>,
    /// Opcode by gas cost bucket counters.
    pub costs: Vec<u32>,
    /// Less frequent events.
    pub events: HashMap<String, u64>,
    /// Open call frames, innermost last.
    pub frames: Vec<CoverageFrame>,
    /// Type of the transaction being executed.
    pub tx_type: u8,
    /// Contracts created by the transaction being executed.
    pub created: HashSet<Address>,
    /// Deepest frame of the transaction being executed.
    pub max_depth: usize,
    /// Opcode of the instruction being executed.
    pub op: u8,
    /// Gas remaining before the instruction being executed.
    pub gas_before: u64,
}

/// What [`Coverage`] tracks about an open call frame.
#[derive(Debug, Default, Clone, Copy)]
pub struct CoverageFrame {
    /// Last opcode the frame executed.
    pub last_op: u8,
    /// Whether the frame ran bytecode.
    pub interpreted: bool,
}

impl Coverage {
    /// Depth buckets in `op:` features.
    pub const DEPTH_BUCKETS: usize = 4;
    /// Gas cost buckets in `gas:` features.
    pub const COST_BUCKETS: usize = 33 + 64;
    /// Errors native precompiles revert with before reading the selector: `NonPayable()` and
    /// `DelegateCallNotAllowed()`.
    pub const PRE_DISPATCH_ERRORS: [&[u8]; 2] =
        [&[0x6f, 0xb1, 0xb0, 0xe9], &[0x0d, 0x89, 0x43, 0x8e]];

    /// Creates a collector for a block under `fork`.
    pub fn new(fork: String) -> Self {
        Self {
            fork,
            ops: vec![0; 256 * Self::DEPTH_BUCKETS],
            costs: vec![0; 256 * Self::COST_BUCKETS],
            ..Default::default()
        }
    }

    /// Starts a transaction of EIP-2718 type `tx_type`.
    pub fn begin_tx(&mut self, tx_type: u8) {
        self.finish_tx();
        self.tx_type = tx_type;
    }

    fn finish_tx(&mut self) {
        if self.max_depth > 0 || !self.created.is_empty() {
            let depth = Self::wide_bucket(self.max_depth);
            self.hit(format!("maxdepth:{depth}"));
        }
        self.max_depth = 0;
        self.created.clear();
        self.frames.clear();
    }

    /// Counts one hit of `feature`.
    pub fn hit(&mut self, feature: String) {
        *self.events.entry(feature).or_default() += 1;
    }

    /// Bucket of a call depth for per-instruction features.
    pub const fn depth_bucket(depth: usize) -> usize {
        match depth {
            0 => 0,
            1 => 1,
            2..=7 => 2,
            _ => 3,
        }
    }

    /// Bucket of a depth or count spanning up to the call depth limit.
    pub const fn wide_bucket(n: usize) -> &'static str {
        match n {
            0 => "0",
            1 => "1",
            2..=3 => "2-3",
            4..=15 => "4-15",
            16..=127 => "16-127",
            128..=1023 => "128-1023",
            _ => "1024+",
        }
    }

    /// Bucket of a gas amount: exact up to 32, then its power of two.
    pub const fn cost_bucket(cost: u64) -> usize {
        if cost <= 32 { cost as usize } else { 33 + (63 - cost.leading_zeros() as usize) }
    }

    fn cost_label(bucket: usize) -> String {
        if bucket <= 32 { bucket.to_string() } else { format!("2^{}", bucket - 33) }
    }

    /// Bucket of an input length: exact for word multiples up to 256 bytes, otherwise its
    /// word count range.
    pub fn len_bucket(len: usize) -> String {
        match len {
            0..=256 if len.is_multiple_of(32) => len.to_string(),
            0..=256 => format!("~{}w", len / 32),
            _ => format!("2^{}", usize::BITS - 1 - len.leading_zeros()),
        }
    }

    fn op_name(op: u8) -> String {
        OpCode::new(op).map_or_else(|| format!("{op:#04x}"), |op| op.as_str().to_owned())
    }

    fn result(result: InstructionResult) -> String {
        format!("{result:?}")
    }

    const fn scheme(scheme: CallScheme) -> &'static str {
        match scheme {
            CallScheme::Call => "call",
            CallScheme::CallCode => "callcode",
            CallScheme::DelegateCall => "delegatecall",
            CallScheme::StaticCall => "staticcall",
        }
    }

    /// Name of a precompile class for `address`, if it is a standard precompile (`pc`) or a
    /// Base native precompile (`np`).
    pub fn precompile_class(address: &Address) -> (&'static str, String) {
        let bytes = address.as_slice();
        if bytes[..18].iter().all(|b| *b == 0) {
            ("pc", format!("{:#x}", u16::from_be_bytes([bytes[18], bytes[19]])))
        } else if bytes[0] == 0xb2 && bytes[1..10].iter().all(|b| *b == 0) {
            ("np", "b20".to_owned())
        } else {
            ("np", format!("{address:#x}"))
        }
    }

    fn frame_end(&mut self, result: InstructionResult) -> (usize, CoverageFrame) {
        let frame = self.frames.pop().unwrap_or_default();
        let depth = self.frames.len();
        if frame.interpreted {
            let op = Self::op_name(frame.last_op);
            self.hit(format!("end:{op}:{}", Self::result(result)));
        }
        (depth, frame)
    }

    fn top_level(&mut self, result: InstructionResult, refunded: i64, spent: u64) {
        let feature = format!("top:t{}:{}", self.tx_type, Self::result(result));
        self.hit(feature);
        let refund = match refunded {
            ..=0 => "none",
            r if r as u64 > spent / 5 => "capped",
            _ => "uncapped",
        };
        self.hit(format!("refund:{refund}"));
    }

    /// The collected features with their hit counts.
    pub fn features(mut self) -> BTreeMap<String, u64> {
        self.finish_tx();
        let mut out: BTreeMap<String, u64> = self.events.into_iter().collect();
        for (index, count) in self.ops.iter().enumerate().filter(|(_, c)| **c > 0) {
            let (op, depth) = (index / Self::DEPTH_BUCKETS, index % Self::DEPTH_BUCKETS);
            let name = format!("op:{}:{}:d{depth}", self.fork, Self::op_name(op as u8));
            out.insert(name, u64::from(*count));
        }
        for (index, count) in self.costs.iter().enumerate().filter(|(_, c)| **c > 0) {
            let (op, bucket) = (index / Self::COST_BUCKETS, index % Self::COST_BUCKETS);
            let name = format!("gas:{}:{}", Self::op_name(op as u8), Self::cost_label(bucket));
            out.insert(name, u64::from(*count));
        }
        out
    }
}

impl<CTX: ContextTr> Inspector<CTX, EthInterpreter> for Coverage {
    fn initialize_interp(&mut self, _: &mut Interpreter<EthInterpreter>, _: &mut CTX) {
        if let Some(frame) = self.frames.last_mut() {
            frame.interpreted = true;
        }
    }

    fn step(&mut self, interp: &mut Interpreter<EthInterpreter>, _: &mut CTX) {
        let op = interp.bytecode.opcode();
        self.op = op;
        self.gas_before = interp.gas.remaining();
        let depth = self.frames.len().saturating_sub(1);
        if let Some(frame) = self.frames.last_mut() {
            frame.last_op = op;
        }
        self.ops[op as usize * Self::DEPTH_BUCKETS + Self::depth_bucket(depth)] += 1;
    }

    fn step_end(&mut self, interp: &mut Interpreter<EthInterpreter>, _: &mut CTX) {
        let cost = self.gas_before.saturating_sub(interp.gas.remaining());
        self.costs[self.op as usize * Self::COST_BUCKETS + Self::cost_bucket(cost)] += 1;
    }

    fn call(&mut self, _: &mut CTX, _: &mut CallInputs) -> Option<CallOutcome> {
        self.frames.push(CoverageFrame::default());
        self.max_depth = self.max_depth.max(self.frames.len() - 1);
        None
    }

    fn call_end(&mut self, context: &mut CTX, inputs: &CallInputs, outcome: &mut CallOutcome) {
        let result = outcome.result.result;
        let (depth, frame) = self.frame_end(result);
        if outcome.was_precompile_called {
            let (kind, name) = Self::precompile_class(&inputs.bytecode_address);
            let status = Self::result(result);
            if kind == "pc" {
                let len = Self::len_bucket(inputs.input.len());
                self.hit(format!("pc:{name}:len{len}:{status}"));
                let gas = Self::cost_label(Self::cost_bucket(outcome.result.gas.total_gas_spent()));
                self.hit(format!("pcgas:{name}:{gas}"));
            } else {
                let input = inputs.input.bytes(context);
                let output = &outcome.result.output;
                // Native precompiles revert with the bare selector for an unknown function, with
                // the selector plus a decode error for undecodable arguments, and with a fixed
                // error before dispatch (value or delegatecall); the selector does not matter
                // for those, so a random selector does not count as new coverage.
                let (selector, error) = match (input.get(..4), result.is_revert()) {
                    (None, _) => (format!("short{}", input.len()), String::new()),
                    (Some(sel), true) if output.as_ref() == sel => {
                        ("*".to_owned(), ":unknownSelector".to_owned())
                    }
                    (Some(_), true)
                        if output.len() == 4
                            && Self::PRE_DISPATCH_ERRORS.contains(&output.as_ref()) =>
                    {
                        ("*".to_owned(), format!(":{}", alloy_primitives::hex::encode(output)))
                    }
                    (Some(sel), true) if output.starts_with(sel) => {
                        (alloy_primitives::hex::encode(sel), ":decodeFailed".to_owned())
                    }
                    (Some(sel), true) => (
                        alloy_primitives::hex::encode(sel),
                        output.get(..4).map_or_else(
                            || ":empty".to_owned(),
                            |s| format!(":{}", alloy_primitives::hex::encode(s)),
                        ),
                    ),
                    (Some(sel), false) => (alloy_primitives::hex::encode(sel), String::new()),
                };
                self.hit(format!("np:{name}:{selector}:{status}{error}"));
            }
        } else {
            let mut feature = format!(
                "call:{}:{}:d{}",
                Self::scheme(inputs.scheme),
                Self::result(result),
                Self::depth_bucket(depth)
            );
            if inputs.transfers_value() {
                feature.push_str(":value");
            }
            if !frame.interpreted {
                feature.push_str(":nocode");
            }
            self.hit(feature);
        }
        if depth == 0 {
            self.top_level(
                result,
                outcome.result.gas.refunded(),
                outcome.result.gas.total_gas_spent(),
            );
        }
    }

    fn create(&mut self, _: &mut CTX, _: &mut CreateInputs) -> Option<CreateOutcome> {
        self.frames.push(CoverageFrame::default());
        self.max_depth = self.max_depth.max(self.frames.len() - 1);
        None
    }

    fn create_end(&mut self, _: &mut CTX, inputs: &CreateInputs, outcome: &mut CreateOutcome) {
        let result = outcome.result.result;
        let (depth, _) = self.frame_end(result);
        let scheme = match inputs.scheme() {
            CreateScheme::Create => "create",
            CreateScheme::Create2 { .. } => "create2",
            CreateScheme::Custom { .. } => "custom",
        };
        let len = Self::len_bucket(inputs.init_code().len());
        self.hit(format!(
            "create:{scheme}:{}:d{}:init{len}",
            Self::result(result),
            Self::depth_bucket(depth)
        ));
        if result.is_ok()
            && let Some(address) = outcome.address
        {
            self.created.insert(address);
        }
        if depth == 0 {
            self.top_level(
                result,
                outcome.result.gas.refunded(),
                outcome.result.gas.total_gas_spent(),
            );
        }
    }

    fn selfdestruct(&mut self, contract: Address, target: Address, value: U256) {
        let beneficiary = if contract == target { "self" } else { "other" };
        let value = if value.is_zero() { "zero" } else { "value" };
        let age = if self.created.contains(&contract) { "new" } else { "old" };
        self.hit(format!("sd:{beneficiary}:{value}:{age}"));
    }
}
