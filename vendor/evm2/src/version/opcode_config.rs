use core::fmt;

use crate::{
    EvmConfig, EvmTypesHost,
    interpreter::{
        instructions as instr, op,
        private::{Instruction, InstructionImplFn},
    },
};

/// Type-specific opcode configuration.
///
/// Stores the static gas table and instruction implementations for a concrete `EvmTypesHost`
/// family. This is a compile-time input used to build the final interpreter dispatch table.
pub struct OpcodeConfig<T: EvmTypesHost> {
    /// Static opcode gas table.
    static_gas_table: StaticGasTable,
    /// Instruction implementations.
    instruction_impls: InstructionImplTable<T>,
    /// Per-opcode revision for dispatch-table rebuild decisions.
    revisions: [u8; 256],
}

pub(crate) struct InstructionInfo<T: EvmTypesHost> {
    pub(crate) instr: InstructionImplFn<T>,
    #[allow(dead_code)]
    pub(crate) dynamic_gas: bool,
}

impl<T: EvmTypesHost> fmt::Debug for OpcodeConfig<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OpcodeConfig").finish_non_exhaustive()
    }
}

impl<T: EvmTypesHost> OpcodeConfig<T> {
    /// Returns the base type-specific opcode config for `C`.
    pub const fn base<C: EvmConfig<T>>() -> Self {
        super::base_opcode_config::<T, C>()
    }

    /// Creates empty type-specific opcode config.
    #[inline]
    pub(super) const fn empty() -> Self {
        Self {
            static_gas_table: StaticGasTable::empty(),
            instruction_impls: InstructionImplTable::empty(),
            revisions: [0; 256],
        }
    }

    /// Returns the static gas cost for `opcode`.
    #[inline(always)]
    pub const fn static_gas(&self, opcode: u8) -> u16 {
        self.static_gas_table.get(opcode)
    }

    /// Returns the dispatch-table revision for `opcode`.
    #[inline(always)]
    pub(crate) const fn revision(&self, opcode: u8) -> u8 {
        self.revisions[opcode as usize]
    }

    /// Sets the static gas cost for `opcode`.
    #[inline(always)]
    pub const fn set_static_gas(&mut self, opcode: u8, cost: u16) {
        assert!(opcode != op::INVALID, "INVALID opcode cannot be overridden");
        if self.static_gas_table.set(opcode, cost) {
            self.bump_revision(opcode);
        }
    }

    /// Returns instruction metadata for `opcode`.
    #[inline(always)]
    pub(crate) const fn instruction(&self, opcode: u8) -> InstructionInfo<T> {
        self.instruction_impls.get(opcode)
    }

    /// Returns whether `opcode` has no instruction implementation in this config.
    #[inline(always)]
    pub(crate) const fn is_unknown_opcode(&self, opcode: u8) -> bool {
        self.instruction_impls.is_unknown(opcode)
    }

    /// Sets the static gas cost and instruction for `opcode`.
    ///
    /// An `I: Instruction` is implemented using the [`#[instruction]`](evm2_macros::instruction)
    /// proc macro.
    #[inline(always)]
    pub const fn set_instruction<I: Instruction<T>>(&mut self, opcode: u8, gas: u16) {
        assert!(opcode != op::INVALID, "INVALID opcode cannot be overridden");
        self.set_static_gas(opcode, gas);
        if self.instruction_impls.set(opcode, I::execute, I::DYNAMIC_GAS) {
            self.bump_revision(opcode);
        }
    }

    #[inline(always)]
    const fn bump_revision(&mut self, opcode: u8) {
        self.revisions[opcode as usize] += 1;
    }
}

struct StaticGasTable {
    table: [u16; 256],
    _align: [usize; 0],
}

impl StaticGasTable {
    /// Creates an empty gas table.
    #[inline]
    const fn empty() -> Self {
        Self { table: [0; 256], _align: [] }
    }

    /// Returns the gas cost for `opcode`.
    #[inline(always)]
    const fn get(&self, opcode: u8) -> u16 {
        self.table[opcode as usize]
    }

    /// Sets the gas cost for `opcode`.
    #[inline(always)]
    const fn set(&mut self, opcode: u8, cost: u16) -> bool {
        let index = opcode as usize;
        if self.table[index] != cost {
            self.table[index] = cost;
            return true;
        }
        false
    }
}

struct InstructionImplTable<T: EvmTypesHost> {
    instrs: [Option<InstructionImplFn<T>>; 256],
    dynamic_gas: [bool; 256],
}

impl<T: EvmTypesHost> InstructionImplTable<T> {
    /// Creates an empty instruction implementation table.
    #[inline]
    const fn empty() -> Self {
        Self { instrs: [None; 256], dynamic_gas: [false; 256] }
    }

    /// Returns the instruction implementation for `opcode`.
    #[inline(always)]
    const fn get(&self, opcode: u8) -> InstructionInfo<T> {
        match self.instrs[opcode as usize] {
            Some(instr) => {
                InstructionInfo { instr, dynamic_gas: self.dynamic_gas[opcode as usize] }
            }
            None => InstructionInfo { instr: instr::invalid::<T>::execute, dynamic_gas: false },
        }
    }

    /// Returns whether `opcode` has no instruction implementation.
    #[inline(always)]
    const fn is_unknown(&self, opcode: u8) -> bool {
        self.instrs[opcode as usize].is_none()
    }

    /// Sets the instruction implementation for `opcode`.
    #[inline(always)]
    const fn set(&mut self, opcode: u8, instr: InstructionImplFn<T>, dynamic_gas: bool) -> bool {
        let index = opcode as usize;
        self.instrs[index] = Some(instr);
        self.dynamic_gas[index] = dynamic_gas;
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::evm::config::BaseEvmTypes;

    #[test]
    #[should_panic(expected = "INVALID opcode cannot be overridden")]
    fn invalid_static_gas_cannot_be_overridden() {
        let mut tables = OpcodeConfig::<BaseEvmTypes>::empty();
        tables.set_static_gas(op::INVALID, 1);
    }

    #[test]
    #[should_panic(expected = "INVALID opcode cannot be overridden")]
    fn invalid_instruction_cannot_be_overridden() {
        let mut tables = OpcodeConfig::<BaseEvmTypes>::empty();
        tables.set_instruction::<instr::stop<BaseEvmTypes>>(op::INVALID, 0);
    }
}
