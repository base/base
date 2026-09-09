use core::ops::Deref;

use base_execution_evm_primitives::B256;
use base_execution_evm_primitives::{Bytecode, utils::read_u16};

use crate::{InstructionResult, InterpreterAction};

#[cfg(feature = "serde")]
mod serde;

/// Extended bytecode structure that wraps base bytecode with additional execution metadata.
#[derive(Debug)]
pub struct ExtBytecode {
    /// The current instruction pointer.
    instruction_pointer: *const u8,
    /// Whether the execution should continue.
    continue_execution: bool,
    /// Bytecode Keccak-256 hash.
    /// This is `None` if it hasn't been calculated yet.
    /// Since it's not necessary for execution, it's not calculated by default.
    bytecode_hash: Option<B256>,
    /// Actions that the EVM should do. It contains return value of the Interpreter or inputs for `CALL` or `CREATE` instructions.
    /// For `RETURN` or `REVERT` instructions it contains the result of the instruction.
    pub action: Option<InterpreterAction>,
    /// The base bytecode.
    base: Bytecode,
}

impl Deref for ExtBytecode {
    type Target = Bytecode;

    fn deref(&self) -> &Self::Target {
        &self.base
    }
}

impl Default for ExtBytecode {
    #[inline]
    fn default() -> Self {
        Self::new(Bytecode::default())
    }
}

impl ExtBytecode {
    /// Create new extended bytecode and set the instruction pointer to the start of the bytecode.
    ///
    /// The bytecode hash will not be calculated.
    #[inline]
    pub fn new(base: Bytecode) -> Self {
        Self::new_with_optional_hash(base, None)
    }

    /// Creates new `ExtBytecode` with the given hash.
    #[inline]
    pub fn new_with_hash(base: Bytecode, hash: B256) -> Self {
        Self::new_with_optional_hash(base, Some(hash))
    }

    /// Creates new `ExtBytecode` with the given hash.
    #[inline]
    pub fn new_with_optional_hash(base: Bytecode, hash: Option<B256>) -> Self {
        let instruction_pointer = base.bytecode_ptr();
        Self {
            base,
            instruction_pointer,
            bytecode_hash: hash,
            action: None,
            continue_execution: true,
        }
    }

    /// Re-calculates the bytecode hash.
    ///
    /// Prefer [`get_or_calculate_hash`](Self::get_or_calculate_hash) if you just need to get the hash.
    #[inline]
    pub fn calculate_hash(&mut self) -> B256 {
        let hash = self.base.hash_slow();
        self.bytecode_hash = Some(hash);
        hash
    }

    /// Returns the bytecode hash.
    #[inline]
    pub const fn hash(&self) -> Option<B256> {
        self.bytecode_hash
    }

    /// Returns the bytecode hash or calculates it if it is not set.
    #[inline]
    pub fn get_or_calculate_hash(&mut self) -> B256 {
        *self.bytecode_hash.get_or_insert_with(
            #[cold]
            || self.base.hash_slow(),
        )
    }
}

impl ExtBytecode {
    /// Returns `true` if the loop should continue.
    #[inline]
    pub fn is_not_end(&self) -> bool {
        self.continue_execution
    }

    /// Sets the `end` flag internally. Action should be taken after.
    #[inline]
    pub fn reset_action(&mut self) {
        self.continue_execution = true;
    }

    /// Set return action.
    #[inline]
    pub fn set_action(&mut self, action: InterpreterAction) {
        debug_assert_eq!(
            !self.continue_execution,
            self.action.is_some(),
            "has_set_action out of sync"
        );
        debug_assert!(
            self.continue_execution,
            "action already set;\nold: {:#?}\nnew: {:#?}",
            self.action, action,
        );
        self.continue_execution = false;
        self.action = Some(action);
    }

    /// Returns the current action.
    #[inline]
    pub fn action(&mut self) -> &mut Option<InterpreterAction> {
        &mut self.action
    }

    /// Is end of the loop.
    #[inline]
    pub fn is_end(&self) -> bool {
        !self.is_not_end()
    }

    /// Returns instruction result
    #[inline]
    pub fn instruction_result(&mut self) -> Option<InstructionResult> {
        self.action().as_ref().and_then(|action| action.instruction_result())
    }
}

impl ExtBytecode {
    /// Relative jumps does not require checking for overflow.
    #[inline]
    pub fn relative_jump(&mut self, offset: isize) {
        self.instruction_pointer = unsafe { self.instruction_pointer.offset(offset) };
    }

    /// Absolute jumps require checking for overflow and if target is a jump destination
    /// from jump table.
    #[inline]
    pub fn absolute_jump(&mut self, offset: usize) {
        self.instruction_pointer = unsafe { self.base.bytes_ref().as_ptr().add(offset) };
    }

    /// Check legacy jump destination from jump table.
    #[inline]
    pub fn is_valid_legacy_jump(&mut self, offset: usize) -> bool {
        let jt = self.base.legacy_jump_table();
        // SAFETY: Only called by legacy bytecode. Panics in debug mode.
        unsafe { jt.unwrap_unchecked() }.is_valid(offset)
    }

    /// Returns instruction opcode.
    #[inline]
    pub fn opcode(&self) -> u8 {
        // SAFETY: `instruction_pointer` always points to bytecode.
        unsafe { *self.instruction_pointer }
    }

    /// Returns current program counter.
    #[inline]
    pub fn pc(&self) -> usize {
        // SAFETY: `instruction_pointer` should be at an offset from the start of the bytes.
        // In practice this is always true unless a caller modifies the `instruction_pointer` field manually.
        unsafe { self.instruction_pointer.offset_from_unsigned(self.base.bytes_ref().as_ptr()) }
    }
}

impl ExtBytecode {
    /// Reads next 16 bits as unsigned integer from the bytecode.
    #[inline]
    pub fn read_u16(&self) -> u16 {
        unsafe { read_u16(self.instruction_pointer) }
    }

    /// Reads next 8 bits as unsigned integer from the bytecode.
    #[inline]
    pub fn read_u8(&self) -> u8 {
        unsafe { *self.instruction_pointer }
    }

    /// Reads next `len` bytes from the bytecode.
    ///
    /// Used by PUSH opcode.
    #[inline]
    pub fn read_slice(&self, len: usize) -> &[u8] {
        unsafe { core::slice::from_raw_parts(self.instruction_pointer, len) }
    }

    /// Reads next 16 bits as unsigned integer from the bytecode at given offset.
    #[inline]
    pub fn read_offset_u16(&self, offset: isize) -> u16 {
        unsafe {
            read_u16(
                self.instruction_pointer
                    // Offset for max_index that is one byte
                    .offset(offset),
            )
        }
    }
}

impl ExtBytecode {
    /// Returns current bytecode original length. Used in [`base_execution_evm_primitives::opcode::CODESIZE`] opcode.
    pub fn bytecode_len(&self) -> usize {
        self.base.len()
    }

    /// Returns current bytecode original slice. Used in [`base_execution_evm_primitives::opcode::CODECOPY`] opcode.
    pub fn bytecode_slice(&self) -> &[u8] {
        self.base.original_byte_slice()
    }
}

#[cfg(test)]
mod tests {
    use base_execution_evm_primitives::Bytes;

    use super::*;

    #[test]
    fn test_with_hash_constructor() {
        let bytecode = Bytecode::new_raw(Bytes::from(&[0x60, 0x00][..]));
        let hash = bytecode.hash_slow();
        let ext_bytecode = ExtBytecode::new_with_hash(bytecode.clone(), hash);
        assert_eq!(ext_bytecode.bytecode_hash, Some(hash));
    }
}
