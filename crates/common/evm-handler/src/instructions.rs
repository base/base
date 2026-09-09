use std::boxed::Box;

use auto_impl::auto_impl;
use revm_interpreter::{
    Host, Instruction,
    instructions::{GasTable, InstructionTable, gas_table_spec},
};
use revm_primitives::hardfork::SpecId;

/// Stores instructions for EVM.
#[auto_impl(&mut, Box)]
pub trait InstructionProvider {
    /// Context type.
    type Context;

    /// Returns the instruction table that is used by EvmTr to execute instructions.
    fn instruction_table(&self) -> &InstructionTable<Self::Context>;

    /// Returns the gas table for static gas costs.
    fn gas_table(&self) -> &GasTable;
}

/// Ethereum instruction contains list of mainnet instructions that is used for Interpreter execution.
#[derive(Debug)]
pub struct EthInstructions<HOST: ?Sized> {
    /// Spec that is used to set gas costs for instructions.
    pub spec: SpecId,
    inner: Box<EthInstructionsInner<HOST>>,
}

#[derive(Debug)]
struct EthInstructionsInner<HOST: ?Sized> {
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
    #[deprecated(since = "0.2.0", note = "use new_mainnet_with_spec instead")]
    pub fn new_mainnet() -> Self {
        let spec = SpecId::default();
        Self::new_mainnet_with_spec(spec)
    }

    /// Returns `EthInstructions` with mainnet spec.
    pub fn new_mainnet_with_spec(spec: SpecId) -> Self {
        Self::new(revm_interpreter::instruction_table(), gas_table_spec(spec), spec)
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

impl<CTX> InstructionProvider for EthInstructions<CTX>
where
    CTX: Host,
{
    type Context = CTX;

    #[inline]
    fn instruction_table(&self) -> &InstructionTable<Self::Context> {
        self.instruction_table()
    }

    #[inline]
    fn gas_table(&self) -> &GasTable {
        self.gas_table()
    }
}
