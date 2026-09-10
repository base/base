use crate::core_machine::Interpreter;

/// Context passed to instruction implementations containing the host and interpreter.
/// This struct provides access to both the host interface for external state operations
/// and the interpreter state for stack, memory, and gas operations.
pub struct InstructionContext<'a, H: ?Sized> {
    /// Reference to the interpreter containing execution state (stack, memory, gas, etc).
    pub interpreter: &'a mut Interpreter,
    /// Reference to the host interface for accessing external blockchain state.
    pub host: &'a mut H,
}

impl<H: ?Sized> std::fmt::Debug for InstructionContext<'_, H> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("InstructionContext")
            .field("host", &"<host>")
            .field("interpreter", &"<interpreter>")
            .finish()
    }
}
