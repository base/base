//! Opcount tracing inspector that simply counts all opcodes.
//!
//! See also <https://geth.ethereum.org/docs/developers/evm-tracing/built-in-tracers>

use revm::{interpreter::Interpreter, Inspector};

/// An inspector that counts all opcodes.
#[derive(Clone, Copy, Debug, Default)]
pub struct OpcodeCountInspector {
    /// opcode counter
    count: usize,
}

impl OpcodeCountInspector {
    /// Returns the opcode counter
    #[inline]
    pub const fn count(&self) -> usize {
        self.count
    }
}

impl<CTX> Inspector<CTX> for OpcodeCountInspector {
    fn step(&mut self, _interp: &mut Interpreter, _context: &mut CTX) {
        self.count += 1;
    }
}
