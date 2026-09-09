use std::boxed::Box;

use base_execution_evm_machine::{
    Host, Instruction,
    instructions::{GasTable, InstructionTable, gas_table_spec},
};
use base_execution_evm_primitives::hardfork::SpecId;

/// Ethereum instruction contains list of mainnet instructions that is used for Interpreter execution.
#[derive(Debug)]
pub struct EthInstructions<HOST: ?Sized> {
    /// Spec that is used to set gas costs for instructions.
    pub spec: SpecId,
    inner: Box<EthInstructionsInner<HOST>>,
}

/// Heap-allocated opcode and gas tables.
#[derive(Debug)]
pub struct EthInstructionsInner<HOST: ?Sized> {
    /// Table containing instruction implementations indexed by opcode.
    instruction_table: InstructionTable<HOST>,
    /// Static gas cost table indexed by opcode.
    gas_table: GasTable,
}

impl<HOST: Host + ?Sized> Clone for EthInstructions<HOST> {
    fn clone(&self) -> Self {
        Self { spec: self.spec, inner: self.inner.clone() }
    }
}

impl<HOST: Host + ?Sized> Clone for EthInstructionsInner<HOST> {
    fn clone(&self) -> Self {
        *self
    }
}
impl<HOST: Host + ?Sized> Copy for EthInstructionsInner<HOST> {}

impl<HOST> EthInstructions<HOST>
where
    HOST: Host,
{
    /// Returns `EthInstructions` with mainnet spec.
    pub fn new_mainnet_with_spec(spec: SpecId) -> Self {
        Self::new(base_execution_evm_machine::instruction_table(), gas_table_spec(spec), spec)
    }

    /// Returns a new instance of `EthInstructions` with custom instruction and gas tables.
    pub fn new(
        instruction_table: InstructionTable<HOST>,
        gas_table: GasTable,
        spec: SpecId,
    ) -> Self {
        Self { spec, inner: Box::new(EthInstructionsInner { instruction_table, gas_table }) }
    }

    /// Inserts a new instruction into the instruction table.
    #[inline]
    pub fn insert_instruction(&mut self, opcode: u8, instruction: Instruction<HOST>, gas: u16) {
        self.inner.instruction_table[opcode as usize] = instruction;
        self.inner.gas_table[opcode as usize] = gas;
    }

    /// Inserts a new gas cost into the gas table.
    #[inline]
    pub fn insert_gas(&mut self, opcode: u8, gas: u16) {
        self.inner.gas_table[opcode as usize] = gas;
    }

    /// Returns a reference to the instruction table.
    #[inline]
    pub fn instruction_table(&self) -> &InstructionTable<HOST> {
        &self.inner.instruction_table
    }

    /// Returns a mutable reference to the instruction table.
    #[inline]
    pub fn instruction_table_mut(&mut self) -> &mut InstructionTable<HOST> {
        &mut self.inner.instruction_table
    }

    /// Returns a reference to the gas table.
    #[inline]
    pub fn gas_table(&self) -> &GasTable {
        &self.inner.gas_table
    }

    /// Returns a mutable reference to the gas table.
    #[inline]
    pub fn gas_table_mut(&mut self) -> &mut GasTable {
        &mut self.inner.gas_table
    }
}
