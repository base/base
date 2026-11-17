//! Generated contract programs that reach deep EVM behavior.
//!
//! A program is a sequence of fragments, each guarded by one bit of the first calldata word, so
//! a single deployed contract behaves differently on every call. Fragments read their operands
//! from calldata parameter words (see [`Param`]), which callers fill from what they know about
//! the chain state, and record what they observe in storage and logs so that the post-state
//! exposes every computed value. The top byte of the first word picks how the call ends.

use alloy_primitives::{Address, Bytes, U256, keccak256};
use base_common_genesis::{BaseFeature, BaseUpgrade};

use crate::r#gen::{
    asm::{Asm, Op},
    precompile::{PrecompileCall, Precompiles},
    rng::Rng,
};

/// Calldata parameter words read by fragments. Word `i` lives at offset `32 * i`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Param {
    /// Storage slot selector (low 4 bits used).
    Slot = 1,
    /// Storage value.
    Value = 2,
    /// Memory offset.
    Offset = 3,
    /// Call target address.
    Target = 4,
    /// Call value in wei.
    CallValue = 5,
    /// Call gas.
    CallGas = 6,
    /// Remaining self-call recursion depth.
    Depth = 7,
    /// CREATE2 salt.
    Salt = 8,
    /// Loop iterations of the gas burner.
    Loops = 9,
    /// Arithmetic operand.
    OperandA = 10,
    /// Arithmetic operand.
    OperandB = 11,
    /// Log topic.
    Topic = 12,
    /// Block hash lookback.
    Lookback = 13,
    /// `SELFDESTRUCT` beneficiary.
    Beneficiary = 14,
    /// Forwarded call target.
    ForwardTarget = 15,
    /// Forwarded call value.
    ForwardValue = 16,
}

impl Param {
    /// Number of parameter words after the control word.
    pub const COUNT: usize = 16;

    /// Calldata offset of the forwarded-call payload that follows the parameter words.
    pub const TAIL_OFFSET: u64 = 32 * (Self::COUNT as u64 + 1);

    const fn offset(self) -> u64 {
        32 * self as u64
    }
}

/// How a call to a program ends, selected by the top byte of the control word.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ending {
    /// `STOP`.
    Stop = 0,
    /// `RETURN` 64 bytes of memory.
    Return = 1,
    /// `REVERT` with 32 bytes of memory.
    Revert = 2,
    /// The designated invalid opcode.
    Invalid = 3,
    /// `SELFDESTRUCT` to [`Param::Beneficiary`].
    SelfDestruct = 4,
    /// An endless loop that runs out of gas.
    OutOfGas = 5,
}

/// A fragment kind, recorded so callers can choose meaningful parameters.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Fragment {
    /// Arithmetic and bitwise operations on edge values.
    Arith,
    /// Block and transaction environment opcodes.
    Env,
    /// `BLOCKHASH` and the EIP-2935/EIP-4788 system contracts.
    History,
    /// Account inspection of other addresses.
    External,
    /// Memory expansion, `MCOPY`, and hashing.
    Memory,
    /// Calldata, code, and returndata copies.
    Copies,
    /// Storage writes with refunds, and transient storage.
    Storage,
    /// Logs.
    Logs,
    /// A call of a fixed kind to [`Param::Target`].
    Call,
    /// Recursive self-call bounded by [`Param::Depth`].
    SelfCall,
    /// A precompile call with a generated input.
    Precompile,
    /// `CREATE` or `CREATE2` of a generated init code.
    Create,
    /// A gas-burning loop bounded by [`Param::Loops`].
    Burn,
    /// A call to [`Param::ForwardTarget`] with the calldata tail as input.
    Forward,
}

/// Context for program generation.
#[derive(Debug, Clone)]
pub struct ProgramCtx {
    /// Rules in effect.
    pub upgrade: BaseUpgrade,
    /// Addresses fragments may hard-code as targets.
    pub targets: Vec<Address>,
}

/// A generated contract.
#[derive(Debug, Clone)]
pub struct Program {
    /// Runtime bytecode.
    pub runtime: Bytes,
    /// Fragment kinds in mask-bit order.
    pub fragments: Vec<Fragment>,
    /// Every runtime the program's `CREATE`s can deploy (for resolving post-state code hashes).
    pub created_runtimes: Vec<Bytes>,
}

/// Small contracts deployed by programs and transactions.
#[derive(Debug)]
pub struct Templates;

impl Templates {
    /// Runtime that stores its call value and calldata size, emits a log, and returns.
    pub fn logger(push0: bool) -> Bytes {
        let mut asm = Asm::new(push0);
        asm.op(Op::CALLVALUE).push_u64(0).op(Op::SSTORE);
        asm.op(Op::CALLDATASIZE).push_u64(0).op(Op::MSTORE);
        asm.op(Op::CALLER).push_u64(32).push_u64(0).op(Op::LOG0 + 1);
        asm.push_u64(32).push_u64(0).op(Op::RETURN);
        asm.finish()
    }

    /// Runtime that writes storage and then reverts with data.
    pub fn reverter(push0: bool) -> Bytes {
        let mut asm = Asm::new(push0);
        asm.push_u64(1).push_u64(7).op(Op::SSTORE);
        asm.mstore(0, U256::from(0xdead));
        asm.push_u64(32).push_u64(0).op(Op::REVERT);
        asm.finish()
    }

    /// Runtime that self-destructs to its caller.
    pub fn destructor(push0: bool) -> Bytes {
        let mut asm = Asm::new(push0);
        asm.op(Op::CALLER).op(Op::SELFDESTRUCT);
        asm.finish()
    }

    /// Runtime that writes `value` to `count` consecutive slots from `start`
    /// (calldata words 0, 1, 2); a zero value clears them, for refunds and trie collapses.
    pub fn filler(push0: bool) -> Bytes {
        let mut asm = Asm::new(push0);
        let top = asm.label();
        let done = asm.label();
        // stack: i(count)
        asm.push_u64(32).op(Op::CALLDATALOAD).push_u64(0x3ff).op(Op::AND);
        asm.place(top);
        asm.op(Op::DUP1).op(Op::ISZERO).jumpi(done);
        // sstore(start + i - 1, value)
        asm.push_u64(64).op(Op::CALLDATALOAD);
        asm.op(Op::DUP2)
            .push_u64(1)
            .op(Op::SWAP1)
            .op(Op::SUB)
            .push_u64(0)
            .op(Op::CALLDATALOAD)
            .op(Op::ADD);
        asm.op(Op::SSTORE);
        asm.push_u64(1).op(Op::SWAP1).op(Op::SUB).jump(top);
        asm.place(done);
        asm.op(Op::STOP);
        asm.finish()
    }

    /// Pushes calldata word `index`.
    fn arg(asm: &mut Asm, index: u64) {
        asm.push_u64(32 * index).op(Op::CALLDATALOAD);
    }

    /// Jumps to `label` when `arg(word) & mask` is zero.
    fn unless_bit(asm: &mut Asm, word: u64, mask: u64, label: usize) {
        Self::arg(asm, word);
        asm.push_u64(mask).op(Op::AND).op(Op::ISZERO).jumpi(label);
    }

    /// Runtime that relays a call: word 0 selects the scheme (`& 3`: CALL, STATICCALL,
    /// DELEGATECALL, CALLCODE; bit 2 propagates failure as a revert), word 1 is the target, word
    /// 2 the value, and the rest of the calldata is the inner input. Returns the inner return
    /// data.
    pub fn relay(push0: bool) -> Bytes {
        let mut asm = Asm::new(push0);
        let after = asm.label();
        // len = calldatasize - 96; calldatacopy(0, 96, len)
        asm.push_u64(96).op(Op::CALLDATASIZE).op(Op::SUB);
        asm.op(Op::DUP1).push_u64(96).push_u64(0).op(Op::CALLDATACOPY);
        // stack: [len]
        let schemes = [Op::CALL, Op::STATICCALL, Op::DELEGATECALL, Op::CALLCODE];
        let labels: Vec<usize> = schemes.iter().map(|_| asm.label()).collect();
        for (index, label) in labels.iter().enumerate().skip(1) {
            asm.push_u64(index as u64);
            Self::arg(&mut asm, 0);
            asm.push_u64(3).op(Op::AND).op(Op::EQ).jumpi(*label);
        }
        for (op, label) in schemes.iter().zip(&labels) {
            asm.place(*label);
            // retSize retOffset argsSize argsOffset [value] target gas
            asm.push_u64(0).push_u64(0).op(Op::DUP3).push_u64(0);
            if matches!(*op, Op::CALL | Op::CALLCODE) {
                Self::arg(&mut asm, 2);
            }
            Self::arg(&mut asm, 1);
            asm.op(Op::GAS).op(*op).jump(after);
        }
        asm.place(after);
        // stack: [len, success]
        let fine = asm.label();
        asm.op(Op::DUP1);
        Self::unless_bit(&mut asm, 0, 4, fine);
        asm.op(Op::DUP1).jumpi(fine);
        asm.op(Op::RETURNDATASIZE).push_u64(0).push_u64(0).op(Op::RETURNDATACOPY);
        asm.op(Op::RETURNDATASIZE).push_u64(0).op(Op::REVERT);
        asm.place(fine);
        asm.op(Op::RETURNDATASIZE).push_u64(0).push_u64(0).op(Op::RETURNDATACOPY);
        asm.op(Op::RETURNDATASIZE).push_u64(0).op(Op::RETURN);
        asm.finish()
    }

    /// Runtime that calls itself recursively: word 0 selects the scheme (`& 3`: CALL,
    /// STATICCALL, DELEGATECALL, CALLCODE; bit 2 reverts every frame after its inner call),
    /// word 1 is the remaining depth, word 2 the gas each call requests (0 for all available,
    /// so the 63/64 rule decides how deep it gets), and a non-zero word 3 stores each inner
    /// call's result at the slot of its depth.
    pub fn recurser(push0: bool) -> Bytes {
        let mut asm = Asm::new(push0);
        let end = asm.label();
        Self::arg(&mut asm, 1);
        asm.op(Op::DUP1).op(Op::ISZERO).jumpi(end);
        // Calldata of the inner call: words 0, n - 1, 2, 3.
        Self::arg(&mut asm, 0);
        asm.push_u64(0).op(Op::MSTORE);
        asm.push_u64(1).op(Op::DUP2).op(Op::SUB).push_u64(32).op(Op::MSTORE);
        Self::arg(&mut asm, 2);
        asm.push_u64(64).op(Op::MSTORE);
        Self::arg(&mut asm, 3);
        asm.push_u64(96).op(Op::MSTORE);
        // stack: [n, gas]
        let have_gas = asm.label();
        Self::arg(&mut asm, 2);
        asm.op(Op::DUP1).jumpi(have_gas).op(Op::POP).op(Op::GAS);
        asm.place(have_gas);
        let after = asm.label();
        let schemes = [Op::CALL, Op::STATICCALL, Op::DELEGATECALL, Op::CALLCODE];
        let labels: Vec<usize> = schemes.iter().map(|_| asm.label()).collect();
        for (index, label) in labels.iter().enumerate().skip(1) {
            asm.push_u64(index as u64);
            Self::arg(&mut asm, 0);
            asm.push_u64(3).op(Op::AND).op(Op::EQ).jumpi(*label);
        }
        for (op, label) in schemes.iter().zip(&labels) {
            asm.place(*label);
            asm.push_u64(0).push_u64(0).push_u64(128).push_u64(0);
            if matches!(*op, Op::CALL | Op::CALLCODE) {
                asm.push_u64(0);
                asm.op(Op::ADDRESS).op(Op::DUP7);
            } else {
                asm.op(Op::ADDRESS).op(Op::DUP6);
            }
            asm.op(*op).jump(after);
        }
        asm.place(after);
        // stack: [n, gas, success]
        let no_store = asm.label();
        Self::arg(&mut asm, 3);
        asm.op(Op::ISZERO).jumpi(no_store);
        asm.op(Op::DUP3).op(Op::SSTORE).push_u64(0);
        asm.place(no_store);
        Self::unless_bit(&mut asm, 0, 4, end);
        asm.push_u64(0).push_u64(0).op(Op::REVERT);
        asm.place(end);
        asm.op(Op::STOP);
        asm.finish()
    }

    /// Init code variants of [`Self::creator`], by kind: deploys a small runtime, reverts,
    /// returns 24577 bytes (EIP-170), returns code starting with `0xEF` (EIP-3541),
    /// self-destructs to itself, self-destructs to its creator, hits `INVALID`, loops out of gas,
    /// deploys empty code, creates a child, returns exactly 24576 bytes, writes storage before
    /// deploying, and deploys runtimes that self-destruct to themselves or to the caller.
    pub fn creator_inits(push0: bool) -> Vec<Vec<u8>> {
        let finish = |asm: Asm| asm.finish().to_vec();
        let runtime = Self::logger(push0);
        let mut inits = vec![Asm::deployer(&runtime, push0).to_vec()];
        let mut asm = Asm::new(push0);
        asm.mstore(0, U256::from(0xdead)).push_u64(32).push_u64(0).op(Op::REVERT);
        inits.push(finish(asm));
        let mut asm = Asm::new(push0);
        asm.push_u64(0x6001).push_u64(0).op(Op::RETURN);
        inits.push(finish(asm));
        let mut asm = Asm::new(push0);
        asm.push_u64(0xef).push_u64(0).op(Op::MSTORE8).push_u64(1).push_u64(0).op(Op::RETURN);
        inits.push(finish(asm));
        let mut asm = Asm::new(push0);
        asm.op(Op::ADDRESS).op(Op::SELFDESTRUCT);
        inits.push(finish(asm));
        let mut asm = Asm::new(push0);
        asm.op(Op::CALLER).op(Op::SELFDESTRUCT);
        inits.push(finish(asm));
        inits.push(vec![Op::INVALID]);
        let mut asm = Asm::new(push0);
        let top = asm.label();
        asm.place(top).jump(top);
        inits.push(finish(asm));
        inits.push(vec![Op::STOP]);
        let mut asm = Asm::new(push0);
        asm.push_u64(0).push_u64(0).push_u64(0).op(Op::CREATE).op(Op::POP);
        asm.push_u64(1).push_u64(0).op(Op::RETURN);
        inits.push(finish(asm));
        let mut asm = Asm::new(push0);
        asm.push_u64(0x6000).push_u64(0).op(Op::RETURN);
        inits.push(finish(asm));
        let mut asm = Asm::new(push0);
        asm.push_u64(1).push_u64(1).op(Op::SSTORE).copy_data(runtime.to_vec(), 0);
        asm.push_u64(runtime.len() as u64).push_u64(0).op(Op::RETURN);
        inits.push(finish(asm));
        // Runtimes that self-destruct when the creator calls them in the same transaction
        // (EIP-6780 deletes them): to themselves, and to the creator.
        inits.push(Asm::deployer(&Self::destructor_to(push0), push0).to_vec());
        inits.push(Asm::deployer(&Self::destructor(push0), push0).to_vec());
        inits
    }

    /// Runtime that creates contracts: word 0 selects CREATE (0) or CREATE2, word 1 is the
    /// CREATE2 salt, word 2 the value each creation endows, word 3 the number of creations
    /// (`& 3`, plus one; repeated CREATE2 salts collide), word 4 the init code kind (see
    /// [`Self::creator_inits`]), word 5 zero padding appended to the init code (`& 0xffff`, so
    /// it can pass the EIP-3860 limit), and a non-zero word 6 calls each new contract. Each
    /// created address is stored at the slot of its creation count.
    pub fn creator(push0: bool) -> Bytes {
        let mut asm = Asm::new(push0);
        let have_init = asm.label();
        let inits = Self::creator_inits(push0);
        for (kind, init) in inits.iter().enumerate() {
            let next = asm.label();
            asm.push_u64(kind as u64);
            Self::arg(&mut asm, 4);
            asm.push_u64(15).op(Op::AND).op(Op::EQ).op(Op::ISZERO).jumpi(next);
            asm.copy_data(init.clone(), 0).push_u64(init.len() as u64).jump(have_init);
            asm.place(next);
        }
        asm.push_u64(0);
        asm.place(have_init);
        // stack: [total]
        Self::arg(&mut asm, 5);
        asm.push_u64(0xffff).op(Op::AND).op(Op::ADD);
        // stack: [total, n]
        Self::arg(&mut asm, 3);
        asm.push_u64(3).op(Op::AND).push_u64(1).op(Op::ADD);
        let top = asm.label();
        let done = asm.label();
        let plain = asm.label();
        let created = asm.label();
        let no_call = asm.label();
        asm.place(top);
        asm.op(Op::DUP1).op(Op::ISZERO).jumpi(done);
        Self::arg(&mut asm, 0);
        asm.op(Op::ISZERO).jumpi(plain);
        Self::arg(&mut asm, 1);
        asm.op(Op::DUP3).push_u64(0);
        Self::arg(&mut asm, 2);
        asm.op(Op::CREATE2).jump(created);
        asm.place(plain);
        asm.op(Op::DUP2).push_u64(0);
        Self::arg(&mut asm, 2);
        asm.op(Op::CREATE);
        asm.place(created);
        // stack: [total, n, address]
        Self::arg(&mut asm, 6);
        asm.op(Op::ISZERO).jumpi(no_call);
        asm.push_u64(0).push_u64(0).push_u64(0).push_u64(0).push_u64(0);
        asm.op(Op::DUP6).op(Op::GAS).op(Op::CALL).op(Op::POP);
        asm.place(no_call);
        asm.op(Op::DUP2).op(Op::SSTORE);
        asm.push_u64(1).op(Op::SWAP1).op(Op::SUB).jump(top);
        asm.place(done);
        asm.op(Op::STOP);
        asm.finish()
    }

    /// Runtime that self-destructs to calldata word 0, or to itself when it is zero.
    pub fn destructor_to(push0: bool) -> Bytes {
        let mut asm = Asm::new(push0);
        let have = asm.label();
        Self::arg(&mut asm, 0);
        asm.op(Op::DUP1).jumpi(have).op(Op::POP).op(Op::ADDRESS);
        asm.place(have);
        asm.op(Op::SELFDESTRUCT);
        asm.finish()
    }

    /// Runtime exercising SSTORE refunds over `count` slots (word 2 `& 31`, plus one) from
    /// word 1, whose original values are non-zero (see `Genesis`). Word 0 bits: 1 clears each
    /// slot, 2 changes and then restores it, 4 clears and then restores it, 8 also writes
    /// transient storage, 16 reverts at the end (discarding the refunds).
    pub fn refunder(push0: bool) -> Bytes {
        let mut asm = Asm::new(push0);
        Self::arg(&mut asm, 2);
        asm.push_u64(31).op(Op::AND).push_u64(1).op(Op::ADD);
        let top = asm.label();
        let done = asm.label();
        asm.place(top);
        asm.op(Op::DUP1).op(Op::ISZERO).jumpi(done);
        // stack: [n, slot, original]
        asm.op(Op::DUP1);
        Self::arg(&mut asm, 1);
        asm.op(Op::ADD).op(Op::DUP1).op(Op::SLOAD);
        let skip_restore = asm.label();
        Self::unless_bit(&mut asm, 0, 2, skip_restore);
        asm.push_u64(1).op(Op::DUP2).op(Op::ADD).op(Op::DUP3).op(Op::SSTORE);
        asm.op(Op::DUP1).op(Op::DUP3).op(Op::SSTORE);
        asm.place(skip_restore);
        let skip_dirty = asm.label();
        Self::unless_bit(&mut asm, 0, 4, skip_dirty);
        asm.push_u64(0).op(Op::DUP3).op(Op::SSTORE);
        asm.op(Op::DUP1).op(Op::DUP3).op(Op::SSTORE);
        asm.place(skip_dirty);
        let skip_clear = asm.label();
        Self::unless_bit(&mut asm, 0, 1, skip_clear);
        asm.push_u64(0).op(Op::DUP3).op(Op::SSTORE);
        asm.place(skip_clear);
        let skip_transient = asm.label();
        Self::unless_bit(&mut asm, 0, 8, skip_transient);
        asm.op(Op::DUP1).op(Op::DUP3).op(Op::TSTORE);
        asm.place(skip_transient);
        asm.op(Op::POP).op(Op::POP);
        asm.push_u64(1).op(Op::SWAP1).op(Op::SUB).jump(top);
        asm.place(done);
        let stop = asm.label();
        Self::unless_bit(&mut asm, 0, 16, stop);
        asm.push_u64(0).push_u64(0).op(Op::REVERT);
        asm.place(stop);
        asm.op(Op::STOP);
        asm.finish()
    }

    /// Every template runtime, in address order (see `Genesis`), for registering code hashes.
    pub fn all(push0: bool) -> Vec<Bytes> {
        vec![
            Self::logger(push0),
            Self::reverter(push0),
            Self::destructor(push0),
            Self::filler(push0),
            Self::relay(push0),
            Self::recurser(push0),
            Self::creator(push0),
            Self::destructor_to(push0),
            Self::refunder(push0),
        ]
    }

    /// Init code variants for `CREATE`, with the runtime each deploys (`None` when the creation
    /// fails or deploys no code).
    pub fn init_code(rng: &mut Rng, push0: bool, upgrade: BaseUpgrade) -> (Vec<u8>, Option<Bytes>) {
        let shanghai = upgrade.is_feature_enabled(BaseFeature::Create2Deployer);
        match rng.below(12) {
            0 => {
                let runtime = Self::logger(push0);
                (Asm::deployer(&runtime, push0).to_vec(), Some(runtime))
            }
            1 => {
                let runtime = Self::reverter(push0);
                (Asm::deployer(&runtime, push0).to_vec(), Some(runtime))
            }
            2 => {
                let runtime = Self::destructor(push0);
                (Asm::deployer(&runtime, push0).to_vec(), Some(runtime))
            }
            3 | 4 => {
                // Runtime of zeros at and just over the EIP-170 code size limit.
                let len = if rng.chance(50) { 24_576 } else { 24_577 };
                let mut asm = Asm::new(push0);
                asm.push_u64(len).push_u64(0).op(Op::RETURN);
                let runtime = (len == 24_576).then(|| Bytes::from(vec![0u8; len as usize]));
                (asm.finish().to_vec(), runtime)
            }
            5 => {
                // EIP-3541: runtime starting with 0xEF is rejected.
                let mut asm = Asm::new(push0);
                asm.mstore(0, U256::from(0xef) << 248)
                    .push_u64(rng.range(1, 33))
                    .push_u64(0)
                    .op(Op::RETURN);
                (asm.finish().to_vec(), None)
            }
            6 => {
                // Constructor that reverts with data.
                let mut asm = Asm::new(push0);
                asm.push_u64(5).push_u64(0).op(Op::SSTORE).push_u64(32).push_u64(0).op(Op::REVERT);
                (asm.finish().to_vec(), None)
            }
            7 => {
                // Self-destruct during creation (EIP-6780 deletes the account).
                let mut asm = Asm::new(push0);
                asm.op(Op::CALLER).op(Op::SELFDESTRUCT);
                (asm.finish().to_vec(), None)
            }
            8 => {
                // Constructor that never finishes.
                let mut asm = Asm::new(push0);
                let top = asm.label();
                asm.place(top).jump(top);
                (asm.finish().to_vec(), None)
            }
            9 => (Vec::new(), None),
            10 if shanghai => {
                // EIP-3860: initcode one byte over the limit (zero memory, run from memory).
                (vec![Op::STOP; 49_153], None)
            }
            _ => {
                // Constructor with storage writes and a log that deploys a logger.
                let runtime = Self::logger(push0);
                let mut asm = Asm::new(push0);
                asm.op(Op::CALLVALUE).push_u64(3).op(Op::SSTORE);
                asm.op(Op::ADDRESS).push_u64(4).op(Op::SSTORE);
                asm.push_u64(0).push_u64(0).op(Op::LOG0);
                asm.copy_data(runtime.to_vec(), 0);
                asm.push_u64(runtime.len() as u64).push_u64(0).op(Op::RETURN);
                (asm.finish().to_vec(), Some(runtime))
            }
        }
    }
}

impl Program {
    /// Generates a program.
    pub fn generate(rng: &mut Rng, ctx: &ProgramCtx) -> Self {
        let push0 = ctx.upgrade.is_feature_enabled(BaseFeature::Create2Deployer);
        let mut asm = Asm::new(push0);
        let mut fragments = Vec::new();
        let mut created_runtimes = Vec::new();
        let count = rng.range(4, 20) as usize;
        for index in 0..count {
            let fragment = Self::pick(rng);
            let skip = asm.label();
            // if (calldata[0] & (1 << index)) == 0 skip
            asm.push_u64(0)
                .op(Op::CALLDATALOAD)
                .push(U256::from(1) << index)
                .op(Op::AND)
                .op(Op::ISZERO)
                .jumpi(skip);
            let slot_base = 0x100 + 0x10 * index as u64;
            Self::emit(rng, ctx, &mut asm, fragment, slot_base, &mut created_runtimes);
            asm.place(skip);
            fragments.push(fragment);
        }
        Self::emit_ending(&mut asm);
        let runtime = asm.finish();
        if runtime.len() > 24_576 {
            // Large precompile inputs can push a program over the EIP-170 limit; retry.
            return Self::generate(rng, ctx);
        }
        Self { runtime, fragments, created_runtimes }
    }

    fn pick(rng: &mut Rng) -> Fragment {
        const KINDS: [(Fragment, u64); 14] = [
            (Fragment::Arith, 10),
            (Fragment::Env, 8),
            (Fragment::History, 4),
            (Fragment::External, 6),
            (Fragment::Memory, 6),
            (Fragment::Copies, 5),
            (Fragment::Storage, 12),
            (Fragment::Logs, 6),
            (Fragment::Call, 10),
            (Fragment::SelfCall, 3),
            (Fragment::Precompile, 14),
            (Fragment::Create, 6),
            (Fragment::Burn, 2),
            (Fragment::Forward, 8),
        ];
        let weights: Vec<u64> = KINDS.iter().map(|(_, w)| *w).collect();
        KINDS[rng.weighted(&weights)].0
    }

    fn param(asm: &mut Asm, param: Param) {
        asm.push_u64(param.offset()).op(Op::CALLDATALOAD);
    }

    fn operand(rng: &mut Rng, asm: &mut Asm) {
        match rng.below(4) {
            0 => Self::param(asm, Param::OperandA),
            1 => Self::param(asm, Param::OperandB),
            _ => {
                let value = rng.edge_u256();
                asm.push(value);
            }
        }
    }

    fn emit(
        rng: &mut Rng,
        ctx: &ProgramCtx,
        asm: &mut Asm,
        fragment: Fragment,
        slot: u64,
        created: &mut Vec<Bytes>,
    ) {
        let cancun = ctx.upgrade.is_feature_enabled(BaseFeature::BeaconRoots);
        match fragment {
            Fragment::Arith => {
                const BINARY: [u8; 21] = [
                    Op::ADD,
                    Op::MUL,
                    Op::SUB,
                    Op::DIV,
                    Op::SDIV,
                    Op::MOD,
                    Op::SMOD,
                    Op::EXP,
                    Op::SIGNEXTEND,
                    Op::LT,
                    Op::GT,
                    Op::SLT,
                    Op::SGT,
                    Op::EQ,
                    Op::AND,
                    Op::OR,
                    Op::XOR,
                    Op::BYTE,
                    Op::SHL,
                    Op::SHR,
                    Op::SAR,
                ];
                let ops = rng.range(1, 6);
                for i in 0..ops {
                    match rng.below(10) {
                        0 => {
                            Self::operand(rng, asm);
                            Self::operand(rng, asm);
                            Self::operand(rng, asm);
                            asm.op(*rng.pick(&[Op::ADDMOD, Op::MULMOD]));
                        }
                        1 => {
                            Self::operand(rng, asm);
                            // CLZ exists from Azul (Osaka); earlier it is an invalid opcode.
                            let unary = if ctx
                                .upgrade
                                .is_feature_enabled(BaseFeature::TransactionGasLimitCap)
                                || rng.chance(5)
                            {
                                *rng.pick(&[Op::NOT, Op::ISZERO, Op::CLZ])
                            } else {
                                *rng.pick(&[Op::NOT, Op::ISZERO])
                            };
                            asm.op(unary);
                        }
                        _ => {
                            Self::operand(rng, asm);
                            Self::operand(rng, asm);
                            asm.op(*rng.pick(&BINARY));
                        }
                    }
                    asm.sstore_top(slot + i);
                }
            }
            Fragment::Env => {
                let mut ops = vec![
                    Op::ADDRESS,
                    Op::ORIGIN,
                    Op::CALLER,
                    Op::CALLVALUE,
                    Op::CALLDATASIZE,
                    Op::CODESIZE,
                    Op::GASPRICE,
                    Op::RETURNDATASIZE,
                    Op::COINBASE,
                    Op::TIMESTAMP,
                    Op::NUMBER,
                    Op::PREVRANDAO,
                    Op::GASLIMIT,
                    Op::CHAINID,
                    Op::SELFBALANCE,
                    Op::BASEFEE,
                    Op::GAS,
                    Op::MSIZE,
                    Op::PC,
                ];
                if cancun || rng.chance(5) {
                    ops.push(Op::BLOBBASEFEE);
                }
                let n = rng.range(2, 8);
                for i in 0..n {
                    let op = *rng.pick(&ops);
                    asm.op(op);
                    asm.push_u64(32 * i).op(Op::MSTORE);
                }
                if cancun && rng.chance(30) {
                    asm.push_u64(rng.below(3)).op(Op::BLOBHASH).push_u64(32 * n).op(Op::MSTORE);
                }
                asm.push_u64(32 * (n + 1)).push_u64(0).op(Op::KECCAK256).sstore_top(slot);
                asm.push_u64(32 * (n + 1)).push_u64(0).op(Op::LOG0);
            }
            Fragment::History => {
                // BLOCKHASH(NUMBER - lookback) for a fixed and a parameterized lookback.
                let fixed = *rng.pick(&[0u64, 1, 2, 3, 255, 256, 257, 1000]);
                asm.push_u64(fixed).op(Op::NUMBER).op(Op::SUB).op(Op::BLOCKHASH).sstore_top(slot);
                Self::param(asm, Param::Lookback);
                asm.op(Op::NUMBER).op(Op::SUB).op(Op::BLOCKHASH).sstore_top(slot + 1);
                // Query the history (EIP-2935) or beacon roots (EIP-4788) contract.
                let (contract, key_op): (Address, u8) = if rng.chance(50) {
                    (
                        alloy_primitives::address!("0x0000F90827F1C53a10cb7A02335B175320002935"),
                        Op::NUMBER,
                    )
                } else {
                    (
                        alloy_primitives::address!("0x000F3df6D732807Ef1319fB7B8bB8522d0Beac02"),
                        Op::TIMESTAMP,
                    )
                };
                Self::param(asm, Param::Lookback);
                asm.op(key_op).op(Op::SUB).push_u64(0).op(Op::MSTORE);
                asm.push_u64(32).push_u64(32).push_u64(32).push_u64(0);
                asm.push(U256::from_be_slice(contract.as_slice())).op(Op::GAS).op(Op::STATICCALL);
                asm.sstore_top(slot + 2);
                asm.push_u64(32).op(Op::MLOAD).sstore_top(slot + 3);
            }
            Fragment::External => {
                let n = rng.range(1, 4);
                for i in 0..n {
                    let target = if rng.chance(30) || ctx.targets.is_empty() {
                        None
                    } else {
                        Some(*rng.pick(&ctx.targets))
                    };
                    let push_target = |asm: &mut Asm| match target {
                        Some(address) => {
                            asm.push(U256::from_be_slice(address.as_slice()));
                        }
                        None => Self::param(asm, Param::Target),
                    };
                    match rng.below(4) {
                        0 => {
                            push_target(asm);
                            asm.op(Op::BALANCE);
                        }
                        1 => {
                            push_target(asm);
                            asm.op(Op::EXTCODESIZE);
                        }
                        2 => {
                            push_target(asm);
                            asm.op(Op::EXTCODEHASH);
                        }
                        _ => {
                            // EXTCODECOPY(target, 0, offset, len) then hash.
                            let len = rng.range(0, 200);
                            asm.push_u64(len).push_u64(rng.below(64)).push_u64(0);
                            push_target(asm);
                            asm.op(Op::EXTCODECOPY);
                            asm.push_u64(len).push_u64(0).op(Op::KECCAK256);
                        }
                    }
                    asm.sstore_top(slot + i);
                }
            }
            Fragment::Memory => {
                let offset = match rng.below(20) {
                    0..=2 => 0,
                    3..=4 => 31,
                    5..=6 => 0xffff,
                    7..=9 => rng.range(0x1_0000, 0x4_0000),
                    10 => 0x1_0000_0000,
                    _ => rng.below(4096),
                };
                if rng.chance(30) {
                    Self::param(asm, Param::Offset);
                    asm.push_u64(0xf_ffff).op(Op::AND);
                } else {
                    asm.push_u64(offset);
                }
                // stack: off
                asm.op(Op::DUP1);
                Self::operand(rng, asm);
                asm.op(Op::SWAP1).op(Op::MSTORE);
                // MSTORE8 just after.
                asm.op(Op::DUP1).push_u64(32).op(Op::ADD);
                Self::operand(rng, asm);
                asm.op(Op::SWAP1).op(Op::MSTORE8);
                if cancun || rng.chance(5) {
                    // MCOPY(dst = off + delta, src = off, len)
                    asm.push_u64(rng.range(0, 200))
                        .op(Op::DUP2)
                        .op(Op::DUP1)
                        .push_u64(rng.range(0, 64))
                        .op(Op::ADD)
                        .op(Op::MCOPY);
                }
                asm.push_u64(rng.range(0, 128)).op(Op::SWAP1).op(Op::KECCAK256).sstore_top(slot);
                asm.op(Op::MSIZE).sstore_top(slot + 1);
            }
            Fragment::Copies => {
                let len = rng.range(0, 160);
                asm.push_u64(len)
                    .push_u64(*rng.pick(&[0u64, 4, 31, 500, 1 << 40]))
                    .push_u64(64)
                    .op(Op::CALLDATACOPY);
                asm.push_u64(len).push_u64(rng.below(400)).push_u64(256).op(Op::CODECOPY);
                // Fresh returndata from the identity precompile, then copy (possibly out of bounds,
                // which reverts the frame).
                let data_len = rng.range(0, 64);
                asm.push_u64(32)
                    .push_u64(512)
                    .push_u64(data_len)
                    .push_u64(64)
                    .push_u64(4)
                    .op(Op::GAS)
                    .op(Op::STATICCALL)
                    .op(Op::POP);
                let copy_len = if rng.chance(85) { rng.range(0, data_len) } else { data_len + 1 };
                asm.push_u64(copy_len).push_u64(0).push_u64(600).op(Op::RETURNDATACOPY);
                asm.push_u64(700).push_u64(0).op(Op::KECCAK256).sstore_top(slot);
            }
            Fragment::Storage => {
                // Slot and value from parameters: callers pick values relative to the current
                // state (no-op writes, clears, restores) to hit every EIP-2200 transition.
                for i in 0..rng.range(1, 4) {
                    Self::param(asm, Param::Value);
                    if i > 0 {
                        asm.push_u64(i).op(Op::MUL);
                    }
                    Self::param(asm, Param::Slot);
                    asm.push_u64(0x0f).op(Op::AND).op(Op::SSTORE);
                }
                if rng.chance(40) {
                    // Write then restore the original value in the same call.
                    asm.push_u64(rng.below(16)).op(Op::DUP1).op(Op::SLOAD).op(Op::SWAP1);
                    asm.op(Op::DUP2).push_u64(1).op(Op::ADD).op(Op::DUP2).op(Op::SSTORE);
                    asm.op(Op::SSTORE);
                }
                if rng.chance(30) {
                    asm.push_u64(0).push_u64(rng.below(16)).op(Op::SSTORE);
                }
                Self::param(asm, Param::Slot);
                asm.push_u64(0x0f).op(Op::AND).op(Op::SLOAD).sstore_top(slot);
                if cancun || rng.chance(5) {
                    Self::param(asm, Param::Value);
                    Self::param(asm, Param::Slot);
                    asm.op(Op::TSTORE);
                    Self::param(asm, Param::Slot);
                    asm.op(Op::TLOAD).sstore_top(slot + 1);
                }
            }
            Fragment::Logs => {
                let topics = rng.range(0, 4);
                let len = rng.range(0, 96);
                for _ in 0..topics {
                    if rng.chance(50) {
                        Self::param(asm, Param::Topic);
                    } else {
                        asm.push(rng.edge_u256());
                    }
                }
                asm.push_u64(len).push_u64(rng.below(64)).op(Op::LOG0 + topics as u8);
            }
            Fragment::Call => {
                let kind = *rng.pick(&[
                    Op::CALL,
                    Op::CALL,
                    Op::STATICCALL,
                    Op::DELEGATECALL,
                    Op::CALLCODE,
                ]);
                let args_len = rng.range(0, 100);
                // retSize retOffset argsSize argsOffset [value] address gas
                asm.push_u64(64).push_u64(0x400).push_u64(args_len).push_u64(0);
                if kind == Op::CALL || kind == Op::CALLCODE {
                    match rng.below(4) {
                        0 => Self::param(asm, Param::CallValue),
                        1 => {
                            asm.op(Op::SELFBALANCE);
                        }
                        2 => {
                            asm.push_u64(1);
                        }
                        _ => {
                            asm.push_u64(0);
                        }
                    }
                }
                if rng.chance(60) || ctx.targets.is_empty() {
                    Self::param(asm, Param::Target);
                } else {
                    asm.push(U256::from_be_slice(rng.pick(&ctx.targets).as_slice()));
                }
                match rng.below(6) {
                    0 => Self::param(asm, Param::CallGas),
                    1 => {
                        asm.push_u64(*rng.pick(&[0u64, 2299, 2300, 2301, 5000, 21000]));
                    }
                    _ => {
                        asm.op(Op::GAS);
                    }
                }
                asm.op(kind).sstore_top(slot);
                asm.op(Op::RETURNDATASIZE).sstore_top(slot + 1);
                asm.push_u64(0x400).op(Op::MLOAD).sstore_top(slot + 2);
            }
            Fragment::SelfCall => {
                // if depth != 0 { copy calldata, decrement depth, call self }
                let skip = asm.label();
                Self::param(asm, Param::Depth);
                asm.op(Op::ISZERO).jumpi(skip);
                asm.op(Op::CALLDATASIZE).push_u64(0).push_u64(0x2000).op(Op::CALLDATACOPY);
                Self::param(asm, Param::Depth);
                asm.push_u64(1)
                    .op(Op::SWAP1)
                    .op(Op::SUB)
                    .push_u64(0x2000 + Param::Depth.offset())
                    .op(Op::MSTORE);
                asm.push_u64(0)
                    .push_u64(0)
                    .op(Op::CALLDATASIZE)
                    .push_u64(0x2000)
                    .push_u64(0)
                    .op(Op::ADDRESS);
                if rng.chance(50) {
                    asm.op(Op::GAS);
                } else {
                    asm.op(Op::GAS).push_u64(2).op(Op::SWAP1).op(Op::DIV);
                }
                asm.op(Op::CALL).sstore_top(slot);
                asm.place(skip);
            }
            Fragment::Precompile => {
                let call = Precompiles::random(rng, ctx.upgrade);
                Self::emit_precompile(rng, asm, &call, slot);
            }
            Fragment::Create => {
                let (init, runtime) = Templates::init_code(rng, asm.push0, ctx.upgrade);
                if let Some(runtime) = runtime {
                    created.push(runtime);
                }
                let len = init.len() as u64;
                if init.iter().any(|b| *b != Op::STOP) {
                    asm.copy_data(init, 0x3000);
                }
                let create2 =
                    ctx.upgrade.is_feature_enabled(BaseFeature::Create2Deployer) || rng.chance(70);
                let times = if create2 && rng.chance(30) { 2 } else { 1 };
                for i in 0..times {
                    if create2 {
                        Self::param(asm, Param::Salt);
                    }
                    asm.push_u64(len).push_u64(0x3000);
                    if rng.chance(30) {
                        Self::param(asm, Param::CallValue);
                    } else {
                        asm.push_u64(0);
                    }
                    asm.op(if create2 { Op::CREATE2 } else { Op::CREATE });
                    // Call the new contract (if any) to exercise it in the same transaction.
                    asm.op(Op::DUP1).sstore_top(slot + i);
                    asm.push_u64(0)
                        .push_u64(0)
                        .push_u64(0)
                        .push_u64(0)
                        .push_u64(0)
                        .op(Op::DUP6)
                        .op(Op::GAS)
                        .op(Op::CALL);
                    asm.sstore_top(slot + 2 + i).op(Op::POP);
                }
            }
            Fragment::Burn => {
                // for i in 0..min(loops, 4096): sload(keccak(i)) — cold reads that burn gas.
                let top = asm.label();
                let done = asm.label();
                Self::param(asm, Param::Loops);
                asm.push_u64(0xfff).op(Op::AND);
                asm.place(top);
                asm.op(Op::DUP1).op(Op::ISZERO).jumpi(done);
                asm.op(Op::DUP1)
                    .push_u64(0)
                    .op(Op::MSTORE)
                    .push_u64(32)
                    .push_u64(0)
                    .op(Op::KECCAK256)
                    .op(Op::SLOAD)
                    .op(Op::POP);
                asm.push_u64(1).op(Op::SWAP1).op(Op::SUB).jump(top);
                asm.place(done);
                asm.op(Op::POP).op(Op::GAS).sstore_top(slot);
            }
            Fragment::Forward => {
                let kind =
                    *rng.pick(&[Op::CALL, Op::CALL, Op::CALL, Op::STATICCALL, Op::DELEGATECALL]);
                // Copy the tail to memory and call the forward target with it.
                asm.push_u64(Param::TAIL_OFFSET).op(Op::CALLDATASIZE).op(Op::SUB);
                asm.op(Op::DUP1).push_u64(Param::TAIL_OFFSET).push_u64(0x5000).op(Op::CALLDATACOPY);
                // stack: tailLen
                asm.push_u64(64).push_u64(0x400).op(Op::DUP3).push_u64(0x5000);
                if kind == Op::CALL {
                    Self::param(asm, Param::ForwardValue);
                }
                Self::param(asm, Param::ForwardTarget);
                asm.op(Op::GAS).op(kind).sstore_top(slot);
                asm.op(Op::POP);
                asm.op(Op::RETURNDATASIZE).sstore_top(slot + 1);
                asm.push_u64(0x400).op(Op::MLOAD).sstore_top(slot + 2);
                asm.push_u64(0x420).op(Op::MLOAD).sstore_top(slot + 3);
            }
        }
    }

    /// Emits a precompile call whose input is assembled in memory at `0x8000`.
    pub fn emit_precompile(rng: &mut Rng, asm: &mut Asm, call: &PrecompileCall, slot: u64) {
        const BASE: u64 = 0x8000;
        if !call.prefix.is_empty() {
            asm.copy_data(call.prefix.clone(), BASE);
        }
        if call.repeat > 0 && !call.unit.is_empty() {
            // for (i = repeat; i != 0; i--) codecopy(BASE + prefix + (i-1)*unit, unit, unitLen)
            let top = asm.label();
            let done = asm.label();
            asm.push_u64(call.repeat as u64);
            asm.place(top);
            asm.op(Op::DUP1).op(Op::ISZERO).jumpi(done);
            asm.push_data(call.unit.clone());
            // stack: i len off -> dest = BASE + prefix + (i-1)*unitLen
            asm.op(Op::DUP3).push_u64(1).op(Op::SWAP1).op(Op::SUB);
            asm.push_u64(call.unit.len() as u64)
                .op(Op::MUL)
                .push_u64(BASE + call.prefix.len() as u64)
                .op(Op::ADD);
            asm.op(Op::CODECOPY);
            asm.push_u64(1).op(Op::SWAP1).op(Op::SUB).jump(top);
            asm.place(done);
            asm.op(Op::POP);
        }
        let len = call.len() as u64;
        let out = BASE + len + 32;
        let static_call = rng.chance(40);
        asm.push_u64(64).push_u64(out).push_u64(len).push_u64(BASE);
        if !static_call {
            asm.push_u64(0);
        }
        asm.push(U256::from_be_slice(call.address.as_slice()));
        match (call.cost, rng.below(4)) {
            (Some(cost), 0) => {
                asm.push_u64(cost);
            }
            (Some(cost), 1) => {
                asm.push_u64(cost.saturating_sub(1));
            }
            _ => {
                asm.op(Op::GAS);
            }
        }
        asm.op(if static_call { Op::STATICCALL } else { Op::CALL }).sstore_top(slot);
        asm.op(Op::RETURNDATASIZE).sstore_top(slot + 1);
        asm.push_u64(64).push_u64(out).op(Op::KECCAK256).sstore_top(slot + 2);
    }

    fn emit_ending(asm: &mut Asm) {
        let labels: Vec<usize> = (0..6).map(|_| asm.label()).collect();
        for (i, label) in labels.iter().enumerate().skip(1) {
            asm.push_u64(0)
                .op(Op::CALLDATALOAD)
                .push_u64(248)
                .op(Op::SHR)
                .push_u64(i as u64)
                .op(Op::EQ)
                .jumpi(*label);
        }
        asm.place(labels[0]).op(Op::STOP);
        asm.place(labels[Ending::Return as usize]).push_u64(64).push_u64(0).op(Op::RETURN);
        asm.place(labels[Ending::Revert as usize]).push_u64(32).push_u64(0).op(Op::REVERT);
        asm.place(labels[Ending::Invalid as usize]).op(Op::INVALID);
        asm.place(labels[Ending::SelfDestruct as usize]);
        Self::param(asm, Param::Beneficiary);
        asm.op(Op::SELFDESTRUCT);
        let spin = labels[Ending::OutOfGas as usize];
        asm.place(spin).jump(spin);
    }

    /// Calldata for a call: a mask of enabled fragments, an ending, parameters, and a tail.
    pub fn calldata(
        mask: u64,
        ending: Ending,
        params: &[U256; Param::COUNT],
        tail: &[u8],
    ) -> Bytes {
        let mut control = U256::from(mask);
        control |= U256::from(ending as u8) << 248;
        let mut out = control.to_be_bytes::<32>().to_vec();
        for param in params {
            out.extend(param.to_be_bytes::<32>());
        }
        out.extend_from_slice(tail);
        out.into()
    }

    /// Picks a fragment mask for a call: usually a few fragments, sometimes all.
    pub const fn random_mask(rng: &mut Rng, fragments: usize) -> u64 {
        let all = if fragments >= 64 { u64::MAX } else { (1u64 << fragments) - 1 };
        match rng.below(5) {
            0 => all,
            1 => 1 << rng.below(fragments as u64),
            _ => rng.next_u64() & all,
        }
    }

    /// The deterministic address of a CREATE deployment.
    pub fn create_address(deployer: Address, nonce: u64) -> Address {
        deployer.create(nonce)
    }

    /// The hash of a runtime.
    pub fn code_hash(runtime: &[u8]) -> alloy_primitives::B256 {
        keccak256(runtime)
    }
}
