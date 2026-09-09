//! EVM opcode implementations.

#[macro_use]
pub mod macros;
/// Arithmetic operations (ADD, SUB, MUL, DIV, etc.).
pub mod arithmetic;
/// Bitwise operations (AND, OR, XOR, NOT, etc.).
pub mod bitwise;
/// Block information instructions (COINBASE, TIMESTAMP, etc.).
pub mod block_info;
/// Contract operations (CALL, CREATE, DELEGATECALL, etc.).
pub mod contract;
/// Control flow instructions (JUMP, JUMPI, REVERT, etc.).
pub mod control;
/// Host environment interactions (SLOAD, SSTORE, LOG, etc.).
pub mod host;
/// Signed 256-bit integer operations.
pub mod i256;
/// Memory operations (MLOAD, MSTORE, MSIZE, etc.).
pub mod memory;
/// Stack operations (PUSH, POP, DUP, SWAP, etc.).
pub mod stack;
/// System information instructions (ADDRESS, CALLER, etc.).
pub mod system;
/// Transaction information instructions (ORIGIN, GASPRICE, etc.).
pub mod tx_info;
/// Utility functions and helpers for instruction implementation.
pub mod utility;

pub use crate::gas;
pub use base_execution_evm_machine::{
    ACCESS_LIST_ADDRESS, ACCESS_LIST_STORAGE_KEY, BASE, BLOCKHASH, CALL_STIPEND, CALLVALUE,
    CODEDEPOSIT, COLD_ACCOUNT_ACCESS_COST, COLD_ACCOUNT_ACCESS_COST_ADDITIONAL, COLD_SLOAD_COST,
    CONDITION_JUMP_GAS, COPY, CREATE, DATA_LOAD_GAS, DATA_LOADN_GAS, EOF_CREATE_GAS, EXP,
    GasTracker, HIGH, INITCODE_WORD_COST, ISTANBUL_SLOAD_GAS, InitialAndFloorGas, JUMPDEST,
    KECCAK256, KECCAK256WORD, LOG, LOGDATA, LOGTOPIC, LOW, MEMORY, MID, NEWACCOUNT,
    NON_ZERO_BYTE_DATA_COST, NON_ZERO_BYTE_DATA_COST_ISTANBUL, NON_ZERO_BYTE_MULTIPLIER,
    NON_ZERO_BYTE_MULTIPLIER_ISTANBUL, REFUND_SSTORE_CLEARS, RETF_GAS, SELFDESTRUCT_REFUND,
    SSTORE_RESET, SSTORE_SET, STANDARD_TOKEN_COST, TOTAL_COST_FLOOR_PER_TOKEN, VERYLOW,
    WARM_SSTORE_RESET, WARM_STORAGE_READ_COST, ZERO, calculate_initial_tx_gas,
    calculate_initial_tx_gas_for_tx, get_tokens_in_calldata, get_tokens_in_calldata_istanbul,
};
use base_execution_evm_primitives::hardfork::SpecId;

use crate::{Host, InstructionContext, InstructionExecResult};

/// EVM opcode function pointer.
#[derive(Debug)]
pub struct Instruction<H: ?Sized> {
    fn_: fn(InstructionContext<'_, H>) -> InstructionExecResult,
}

impl<H: Host + ?Sized> Instruction<H> {
    /// Creates a new instruction with the given function.
    #[inline]
    pub const fn new(fn_: fn(InstructionContext<'_, H>) -> InstructionExecResult) -> Self {
        Self { fn_ }
    }

    /// Creates an unknown/invalid instruction.
    #[inline]
    pub const fn unknown() -> Self {
        Self { fn_: control::unknown }
    }

    /// Executes the instruction with the given context.
    #[inline(always)]
    pub fn execute(self, ctx: InstructionContext<'_, H>) -> InstructionExecResult {
        (self.fn_)(ctx)
    }
}

impl<H: Host + ?Sized> Copy for Instruction<H> {}
impl<H: Host + ?Sized> Clone for Instruction<H> {
    fn clone(&self) -> Self {
        *self
    }
}

/// Instruction table is list of instruction function pointers mapped to 256 EVM opcodes.
pub type InstructionTable<H> = [Instruction<H>; 256];

/// Static gas cost table mapped to 256 EVM opcodes.
pub type GasTable = [u16; 256];

/// Returns the default instruction table for the given host.
#[inline]
pub const fn instruction_table<H: Host>() -> InstructionTable<H> {
    const { instruction_table_impl::<H>() }
}

/// Returns the default gas table.
#[inline]
pub const fn gas_table() -> GasTable {
    const { gas_table_impl() }
}

/// Create a gas table with applied spec changes to static gas cost.
#[inline]
pub const fn gas_table_spec(spec: SpecId) -> GasTable {
    use SpecId::*;
    use base_execution_evm_primitives::opcode::*;
    let mut table = gas_table();

    if spec.is_enabled_in(TANGERINE) {
        // EIP-150: Gas cost changes for IO-heavy operations
        table[SLOAD as usize] = 200;
        table[BALANCE as usize] = 400;
        table[EXTCODESIZE as usize] = 700;
        table[EXTCODECOPY as usize] = 700;
        table[CALL as usize] = 700;
        table[CALLCODE as usize] = 700;
        table[DELEGATECALL as usize] = 700;
        table[STATICCALL as usize] = 700;
        table[SELFDESTRUCT as usize] = 5000;
    }

    if spec.is_enabled_in(ISTANBUL) {
        // EIP-1884: Repricing for trie-size-dependent opcodes
        table[SLOAD as usize] = gas::ISTANBUL_SLOAD_GAS as u16;
        table[BALANCE as usize] = 700;
        table[EXTCODEHASH as usize] = 700;
    }

    if spec.is_enabled_in(BERLIN) {
        // warm account cost is base gas that is spend. Additional gas depends if account is cold loaded.
        table[SLOAD as usize] = gas::WARM_STORAGE_READ_COST as u16;
        table[BALANCE as usize] = gas::WARM_STORAGE_READ_COST as u16;
        table[EXTCODESIZE as usize] = gas::WARM_STORAGE_READ_COST as u16;
        table[EXTCODEHASH as usize] = gas::WARM_STORAGE_READ_COST as u16;
        table[EXTCODECOPY as usize] = gas::WARM_STORAGE_READ_COST as u16;
        table[CALL as usize] = gas::WARM_STORAGE_READ_COST as u16;
        table[CALLCODE as usize] = gas::WARM_STORAGE_READ_COST as u16;
        table[DELEGATECALL as usize] = gas::WARM_STORAGE_READ_COST as u16;
        table[STATICCALL as usize] = gas::WARM_STORAGE_READ_COST as u16;
    }

    if spec.is_enabled_in(AMSTERDAM) {
        // EIP-8038 §"EXT* family update": EXTCODESIZE and EXTCODECOPY perform two
        // database reads (load the account object, then read its code), so their
        // per-access base is charged an extra WARM_ACCESS on top of the normal
        // account access. WARM_ACCESS itself is unchanged by EIP-8038 (100), so
        // every other access opcode keeps its Berlin warm base; only these two
        // change. The dynamic cold premium is still added by `load_account`.
        let warm = base_execution_evm_primitives::eip8038::WARM_ACCESS as u16;
        table[EXTCODESIZE as usize] = warm + warm;
        table[EXTCODECOPY as usize] = warm + warm;
    }

    table
}

const fn instruction_table_impl<H: Host>() -> InstructionTable<H> {
    use base_execution_evm_primitives::opcode::*;
    let mut table = [Instruction::unknown(); 256];

    table[STOP as usize] = Instruction::new(control::stop);
    table[ADD as usize] = Instruction::new(arithmetic::add);
    table[MUL as usize] = Instruction::new(arithmetic::mul);
    table[SUB as usize] = Instruction::new(arithmetic::sub);
    table[DIV as usize] = Instruction::new(arithmetic::div);
    table[SDIV as usize] = Instruction::new(arithmetic::sdiv);
    table[MOD as usize] = Instruction::new(arithmetic::rem);
    table[SMOD as usize] = Instruction::new(arithmetic::smod);
    table[ADDMOD as usize] = Instruction::new(arithmetic::addmod);
    table[MULMOD as usize] = Instruction::new(arithmetic::mulmod);
    table[EXP as usize] = Instruction::new(arithmetic::exp);
    table[SIGNEXTEND as usize] = Instruction::new(arithmetic::signextend);

    table[LT as usize] = Instruction::new(bitwise::lt);
    table[GT as usize] = Instruction::new(bitwise::gt);
    table[SLT as usize] = Instruction::new(bitwise::slt);
    table[SGT as usize] = Instruction::new(bitwise::sgt);
    table[EQ as usize] = Instruction::new(bitwise::eq);
    table[ISZERO as usize] = Instruction::new(bitwise::iszero);
    table[AND as usize] = Instruction::new(bitwise::bitand);
    table[OR as usize] = Instruction::new(bitwise::bitor);
    table[XOR as usize] = Instruction::new(bitwise::bitxor);
    table[NOT as usize] = Instruction::new(bitwise::not);
    table[BYTE as usize] = Instruction::new(bitwise::byte);
    table[SHL as usize] = Instruction::new(bitwise::shl);
    table[SHR as usize] = Instruction::new(bitwise::shr);
    table[SAR as usize] = Instruction::new(bitwise::sar);
    table[CLZ as usize] = Instruction::new(bitwise::clz);

    table[KECCAK256 as usize] = Instruction::new(system::keccak256);

    table[ADDRESS as usize] = Instruction::new(system::address);
    table[BALANCE as usize] = Instruction::new(host::balance);
    table[ORIGIN as usize] = Instruction::new(tx_info::origin);
    table[CALLER as usize] = Instruction::new(system::caller);
    table[CALLVALUE as usize] = Instruction::new(system::callvalue);
    table[CALLDATALOAD as usize] = Instruction::new(system::calldataload);
    table[CALLDATASIZE as usize] = Instruction::new(system::calldatasize);
    table[CALLDATACOPY as usize] = Instruction::new(system::calldatacopy);
    table[CODESIZE as usize] = Instruction::new(system::codesize);
    table[CODECOPY as usize] = Instruction::new(system::codecopy);

    table[GASPRICE as usize] = Instruction::new(tx_info::gasprice);
    table[EXTCODESIZE as usize] = Instruction::new(host::extcodesize);
    table[EXTCODECOPY as usize] = Instruction::new(host::extcodecopy);
    table[RETURNDATASIZE as usize] = Instruction::new(system::returndatasize);
    table[RETURNDATACOPY as usize] = Instruction::new(system::returndatacopy);
    table[EXTCODEHASH as usize] = Instruction::new(host::extcodehash);
    table[BLOCKHASH as usize] = Instruction::new(host::blockhash);
    table[COINBASE as usize] = Instruction::new(block_info::coinbase);
    table[TIMESTAMP as usize] = Instruction::new(block_info::timestamp);
    table[NUMBER as usize] = Instruction::new(block_info::block_number);
    table[DIFFICULTY as usize] = Instruction::new(block_info::difficulty);
    table[GASLIMIT as usize] = Instruction::new(block_info::gaslimit);
    table[CHAINID as usize] = Instruction::new(block_info::chainid);
    table[SELFBALANCE as usize] = Instruction::new(host::selfbalance);
    table[BASEFEE as usize] = Instruction::new(block_info::basefee);
    table[BLOBHASH as usize] = Instruction::new(tx_info::blob_hash);
    table[BLOBBASEFEE as usize] = Instruction::new(block_info::blob_basefee);
    table[SLOTNUM as usize] = Instruction::new(block_info::slot_num);

    table[POP as usize] = Instruction::new(stack::pop);
    table[MLOAD as usize] = Instruction::new(memory::mload);
    table[MSTORE as usize] = Instruction::new(memory::mstore);
    table[MSTORE8 as usize] = Instruction::new(memory::mstore8);
    table[SLOAD as usize] = Instruction::new(host::sload);
    table[SSTORE as usize] = Instruction::new(host::sstore);
    table[JUMP as usize] = Instruction::new(control::jump);
    table[JUMPI as usize] = Instruction::new(control::jumpi);
    table[PC as usize] = Instruction::new(control::pc);
    table[MSIZE as usize] = Instruction::new(memory::msize);
    table[GAS as usize] = Instruction::new(system::gas);
    table[JUMPDEST as usize] = Instruction::new(control::jumpdest);
    table[TLOAD as usize] = Instruction::new(host::tload);
    table[TSTORE as usize] = Instruction::new(host::tstore);
    table[MCOPY as usize] = Instruction::new(memory::mcopy);

    table[PUSH0 as usize] = Instruction::new(stack::push0);
    table[PUSH1 as usize] = Instruction::new(stack::push::<1, _>);
    table[PUSH2 as usize] = Instruction::new(stack::push::<2, _>);
    table[PUSH3 as usize] = Instruction::new(stack::push::<3, _>);
    table[PUSH4 as usize] = Instruction::new(stack::push::<4, _>);
    table[PUSH5 as usize] = Instruction::new(stack::push::<5, _>);
    table[PUSH6 as usize] = Instruction::new(stack::push::<6, _>);
    table[PUSH7 as usize] = Instruction::new(stack::push::<7, _>);
    table[PUSH8 as usize] = Instruction::new(stack::push::<8, _>);
    table[PUSH9 as usize] = Instruction::new(stack::push::<9, _>);
    table[PUSH10 as usize] = Instruction::new(stack::push::<10, _>);
    table[PUSH11 as usize] = Instruction::new(stack::push::<11, _>);
    table[PUSH12 as usize] = Instruction::new(stack::push::<12, _>);
    table[PUSH13 as usize] = Instruction::new(stack::push::<13, _>);
    table[PUSH14 as usize] = Instruction::new(stack::push::<14, _>);
    table[PUSH15 as usize] = Instruction::new(stack::push::<15, _>);
    table[PUSH16 as usize] = Instruction::new(stack::push::<16, _>);
    table[PUSH17 as usize] = Instruction::new(stack::push::<17, _>);
    table[PUSH18 as usize] = Instruction::new(stack::push::<18, _>);
    table[PUSH19 as usize] = Instruction::new(stack::push::<19, _>);
    table[PUSH20 as usize] = Instruction::new(stack::push::<20, _>);
    table[PUSH21 as usize] = Instruction::new(stack::push::<21, _>);
    table[PUSH22 as usize] = Instruction::new(stack::push::<22, _>);
    table[PUSH23 as usize] = Instruction::new(stack::push::<23, _>);
    table[PUSH24 as usize] = Instruction::new(stack::push::<24, _>);
    table[PUSH25 as usize] = Instruction::new(stack::push::<25, _>);
    table[PUSH26 as usize] = Instruction::new(stack::push::<26, _>);
    table[PUSH27 as usize] = Instruction::new(stack::push::<27, _>);
    table[PUSH28 as usize] = Instruction::new(stack::push::<28, _>);
    table[PUSH29 as usize] = Instruction::new(stack::push::<29, _>);
    table[PUSH30 as usize] = Instruction::new(stack::push::<30, _>);
    table[PUSH31 as usize] = Instruction::new(stack::push::<31, _>);
    table[PUSH32 as usize] = Instruction::new(stack::push::<32, _>);

    table[DUP1 as usize] = Instruction::new(stack::dup::<1, _>);
    table[DUP2 as usize] = Instruction::new(stack::dup::<2, _>);
    table[DUP3 as usize] = Instruction::new(stack::dup::<3, _>);
    table[DUP4 as usize] = Instruction::new(stack::dup::<4, _>);
    table[DUP5 as usize] = Instruction::new(stack::dup::<5, _>);
    table[DUP6 as usize] = Instruction::new(stack::dup::<6, _>);
    table[DUP7 as usize] = Instruction::new(stack::dup::<7, _>);
    table[DUP8 as usize] = Instruction::new(stack::dup::<8, _>);
    table[DUP9 as usize] = Instruction::new(stack::dup::<9, _>);
    table[DUP10 as usize] = Instruction::new(stack::dup::<10, _>);
    table[DUP11 as usize] = Instruction::new(stack::dup::<11, _>);
    table[DUP12 as usize] = Instruction::new(stack::dup::<12, _>);
    table[DUP13 as usize] = Instruction::new(stack::dup::<13, _>);
    table[DUP14 as usize] = Instruction::new(stack::dup::<14, _>);
    table[DUP15 as usize] = Instruction::new(stack::dup::<15, _>);
    table[DUP16 as usize] = Instruction::new(stack::dup::<16, _>);

    table[SWAP1 as usize] = Instruction::new(stack::swap::<1, _>);
    table[SWAP2 as usize] = Instruction::new(stack::swap::<2, _>);
    table[SWAP3 as usize] = Instruction::new(stack::swap::<3, _>);
    table[SWAP4 as usize] = Instruction::new(stack::swap::<4, _>);
    table[SWAP5 as usize] = Instruction::new(stack::swap::<5, _>);
    table[SWAP6 as usize] = Instruction::new(stack::swap::<6, _>);
    table[SWAP7 as usize] = Instruction::new(stack::swap::<7, _>);
    table[SWAP8 as usize] = Instruction::new(stack::swap::<8, _>);
    table[SWAP9 as usize] = Instruction::new(stack::swap::<9, _>);
    table[SWAP10 as usize] = Instruction::new(stack::swap::<10, _>);
    table[SWAP11 as usize] = Instruction::new(stack::swap::<11, _>);
    table[SWAP12 as usize] = Instruction::new(stack::swap::<12, _>);
    table[SWAP13 as usize] = Instruction::new(stack::swap::<13, _>);
    table[SWAP14 as usize] = Instruction::new(stack::swap::<14, _>);
    table[SWAP15 as usize] = Instruction::new(stack::swap::<15, _>);
    table[SWAP16 as usize] = Instruction::new(stack::swap::<16, _>);

    table[DUPN as usize] = Instruction::new(stack::dupn);
    table[SWAPN as usize] = Instruction::new(stack::swapn);
    table[EXCHANGE as usize] = Instruction::new(stack::exchange);

    table[LOG0 as usize] = Instruction::new(host::log::<0, _>);
    table[LOG1 as usize] = Instruction::new(host::log::<1, _>);
    table[LOG2 as usize] = Instruction::new(host::log::<2, _>);
    table[LOG3 as usize] = Instruction::new(host::log::<3, _>);
    table[LOG4 as usize] = Instruction::new(host::log::<4, _>);

    table[CREATE as usize] = Instruction::new(contract::create::<false, _>);
    table[CALL as usize] = Instruction::new(contract::call::<CALL, _>);
    table[CALLCODE as usize] = Instruction::new(contract::call::<CALLCODE, _>);
    table[RETURN as usize] = Instruction::new(control::ret);
    table[DELEGATECALL as usize] = Instruction::new(contract::call::<DELEGATECALL, _>);
    table[CREATE2 as usize] = Instruction::new(contract::create::<true, _>);

    table[STATICCALL as usize] = Instruction::new(contract::call::<STATICCALL, _>);
    table[REVERT as usize] = Instruction::new(control::revert);
    table[INVALID as usize] = Instruction::new(control::invalid);
    table[SELFDESTRUCT as usize] = Instruction::new(host::selfdestruct);
    table
}

const fn gas_table_impl() -> GasTable {
    use base_execution_evm_primitives::opcode::*;
    let mut table = [0u16; 256];

    table[STOP as usize] = 0;
    table[ADD as usize] = 3;
    table[MUL as usize] = 5;
    table[SUB as usize] = 3;
    table[DIV as usize] = 5;
    table[SDIV as usize] = 5;
    table[MOD as usize] = 5;
    table[SMOD as usize] = 5;
    table[ADDMOD as usize] = 8;
    table[MULMOD as usize] = 8;
    table[EXP as usize] = gas::EXP as u16; // base
    table[SIGNEXTEND as usize] = 5;

    table[LT as usize] = 3;
    table[GT as usize] = 3;
    table[SLT as usize] = 3;
    table[SGT as usize] = 3;
    table[EQ as usize] = 3;
    table[ISZERO as usize] = 3;
    table[AND as usize] = 3;
    table[OR as usize] = 3;
    table[XOR as usize] = 3;
    table[NOT as usize] = 3;
    table[BYTE as usize] = 3;
    table[SHL as usize] = 3;
    table[SHR as usize] = 3;
    table[SAR as usize] = 3;
    table[CLZ as usize] = 5;

    table[KECCAK256 as usize] = gas::KECCAK256 as u16;

    table[ADDRESS as usize] = 2;
    table[BALANCE as usize] = 20;
    table[ORIGIN as usize] = 2;
    table[CALLER as usize] = 2;
    table[CALLVALUE as usize] = 2;
    table[CALLDATALOAD as usize] = 3;
    table[CALLDATASIZE as usize] = 2;
    table[CALLDATACOPY as usize] = 3;
    table[CODESIZE as usize] = 2;
    table[CODECOPY as usize] = 3;

    table[GASPRICE as usize] = 2;
    table[EXTCODESIZE as usize] = 20;
    table[EXTCODECOPY as usize] = 20;
    table[RETURNDATASIZE as usize] = 2;
    table[RETURNDATACOPY as usize] = 3;
    table[EXTCODEHASH as usize] = 400;
    table[BLOCKHASH as usize] = 20;
    table[COINBASE as usize] = 2;
    table[TIMESTAMP as usize] = 2;
    table[NUMBER as usize] = 2;
    table[DIFFICULTY as usize] = 2;
    table[GASLIMIT as usize] = 2;
    table[CHAINID as usize] = 2;
    table[SELFBALANCE as usize] = 5;
    table[BASEFEE as usize] = 2;
    table[BLOBHASH as usize] = 3;
    table[BLOBBASEFEE as usize] = 2;
    table[SLOTNUM as usize] = 2;

    table[POP as usize] = 2;
    table[MLOAD as usize] = 3;
    table[MSTORE as usize] = 3;
    table[MSTORE8 as usize] = 3;
    table[SLOAD as usize] = 50;
    // SSTORE static gas can be found in GasParams as check for minimal stipend
    // needs to be done before deduction of static gas.
    table[SSTORE as usize] = 0;
    table[JUMP as usize] = 8;
    table[JUMPI as usize] = 10;
    table[PC as usize] = 2;
    table[MSIZE as usize] = 2;
    table[GAS as usize] = 2;
    table[JUMPDEST as usize] = 1;
    table[TLOAD as usize] = 100;
    table[TSTORE as usize] = 100;
    table[MCOPY as usize] = 3; // static 2, mostly dynamic

    table[PUSH0 as usize] = 2;
    table[PUSH1 as usize] = 3;
    table[PUSH2 as usize] = 3;
    table[PUSH3 as usize] = 3;
    table[PUSH4 as usize] = 3;
    table[PUSH5 as usize] = 3;
    table[PUSH6 as usize] = 3;
    table[PUSH7 as usize] = 3;
    table[PUSH8 as usize] = 3;
    table[PUSH9 as usize] = 3;
    table[PUSH10 as usize] = 3;
    table[PUSH11 as usize] = 3;
    table[PUSH12 as usize] = 3;
    table[PUSH13 as usize] = 3;
    table[PUSH14 as usize] = 3;
    table[PUSH15 as usize] = 3;
    table[PUSH16 as usize] = 3;
    table[PUSH17 as usize] = 3;
    table[PUSH18 as usize] = 3;
    table[PUSH19 as usize] = 3;
    table[PUSH20 as usize] = 3;
    table[PUSH21 as usize] = 3;
    table[PUSH22 as usize] = 3;
    table[PUSH23 as usize] = 3;
    table[PUSH24 as usize] = 3;
    table[PUSH25 as usize] = 3;
    table[PUSH26 as usize] = 3;
    table[PUSH27 as usize] = 3;
    table[PUSH28 as usize] = 3;
    table[PUSH29 as usize] = 3;
    table[PUSH30 as usize] = 3;
    table[PUSH31 as usize] = 3;
    table[PUSH32 as usize] = 3;

    table[DUP1 as usize] = 3;
    table[DUP2 as usize] = 3;
    table[DUP3 as usize] = 3;
    table[DUP4 as usize] = 3;
    table[DUP5 as usize] = 3;
    table[DUP6 as usize] = 3;
    table[DUP7 as usize] = 3;
    table[DUP8 as usize] = 3;
    table[DUP9 as usize] = 3;
    table[DUP10 as usize] = 3;
    table[DUP11 as usize] = 3;
    table[DUP12 as usize] = 3;
    table[DUP13 as usize] = 3;
    table[DUP14 as usize] = 3;
    table[DUP15 as usize] = 3;
    table[DUP16 as usize] = 3;

    table[SWAP1 as usize] = 3;
    table[SWAP2 as usize] = 3;
    table[SWAP3 as usize] = 3;
    table[SWAP4 as usize] = 3;
    table[SWAP5 as usize] = 3;
    table[SWAP6 as usize] = 3;
    table[SWAP7 as usize] = 3;
    table[SWAP8 as usize] = 3;
    table[SWAP9 as usize] = 3;
    table[SWAP10 as usize] = 3;
    table[SWAP11 as usize] = 3;
    table[SWAP12 as usize] = 3;
    table[SWAP13 as usize] = 3;
    table[SWAP14 as usize] = 3;
    table[SWAP15 as usize] = 3;
    table[SWAP16 as usize] = 3;

    table[DUPN as usize] = 3;
    table[SWAPN as usize] = 3;
    table[EXCHANGE as usize] = 3;

    table[LOG0 as usize] = gas::LOG as u16;
    table[LOG1 as usize] = gas::LOG as u16;
    table[LOG2 as usize] = gas::LOG as u16;
    table[LOG3 as usize] = gas::LOG as u16;
    table[LOG4 as usize] = gas::LOG as u16;

    table[CREATE as usize] = 0;
    table[CALL as usize] = 40;
    table[CALLCODE as usize] = 40;
    table[RETURN as usize] = 0;
    table[DELEGATECALL as usize] = 40;
    table[CREATE2 as usize] = 0;

    table[STATICCALL as usize] = 40;
    table[REVERT as usize] = 0;
    table[INVALID as usize] = 0;
    table[SELFDESTRUCT as usize] = 0;
    table
}

#[cfg(test)]
mod tests {
    use base_execution_evm_primitives::opcode::*;

    use super::instruction_table;
    use crate::DummyHost;

    #[test]
    fn all_instructions_and_opcodes_used() {
        // known unknown instruction we compare it with other instructions from table.
        let unknown_instruction = 0x0C_usize;
        let instr_table = instruction_table::<DummyHost>();

        let unknown_istr = instr_table[unknown_instruction];
        for (i, instr) in instr_table.iter().enumerate() {
            let is_opcode_unknown = OpCode::new(i as u8).is_none();
            //
            let is_instr_unknown = std::ptr::fn_addr_eq(instr.fn_, unknown_istr.fn_);
            assert_eq!(is_instr_unknown, is_opcode_unknown, "Opcode 0x{i:X?} is not handled",);
        }
    }

    #[test]
    fn amsterdam_eip8038_ext_family_second_read() {
        use base_execution_evm_primitives::hardfork::SpecId;

        use super::gas_table_spec;

        let warm = base_execution_evm_primitives::eip8038::WARM_ACCESS as u16;
        let table = gas_table_spec(SpecId::AMSTERDAM);
        // EIP-8038 §"EXT* family update": EXTCODESIZE / EXTCODECOPY make a second
        // database read, charged an extra WARM_ACCESS on the static base.
        assert_eq!(table[EXTCODESIZE as usize], warm + warm);
        assert_eq!(table[EXTCODECOPY as usize], warm + warm);
        // Other account-access opcodes keep a single WARM_ACCESS base.
        assert_eq!(table[EXTCODEHASH as usize], warm);
        assert_eq!(table[BALANCE as usize], warm);
        assert_eq!(table[SLOAD as usize], warm);
        // The surcharge is Amsterdam-only: pre-Amsterdam EXTCODESIZE == EXTCODEHASH.
        let prague = gas_table_spec(SpecId::PRAGUE);
        assert_eq!(prague[EXTCODESIZE as usize], prague[EXTCODEHASH as usize]);
    }
}
