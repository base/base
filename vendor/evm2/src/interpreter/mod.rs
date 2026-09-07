//! EVM interpreter.

pub mod gas;
pub use gas::{Gas, GasTracker, MemoryGas};

#[macro_use]
mod utils;

pub(crate) mod instructions;
pub use instructions::i256;

pub(crate) mod dispatch;

#[doc(hidden)] // For macro only. Not public API.
pub mod private;

pub mod opcode;
pub use opcode::op;

mod ctrl;
pub use ctrl::{BytecodeRef, Pc};

mod stack;
pub(crate) use stack::StackBacking;
pub use stack::{Stack, StackMut, StackRef, Word};

mod memory;
pub use memory::Memory;

mod message;
pub use message::{Message, MessageExt, MessageKind, derive_create_destination};

mod host;
pub use host::{Host, MessageResult, MessageResultExt};

mod runtime;
pub(crate) use runtime::InterpreterPool;
pub use runtime::{Interpreter, InterpreterState};

/// Instruction result type.
pub type Result<T = (), E = InstrStop> = core::result::Result<T, E>;

/// Result of executing an EVM instruction.
#[repr(u8)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[non_exhaustive]
pub enum InstrStop {
    /// Encountered a `STOP` opcode
    #[default]
    Stop = 1, // Start at 1 so that `Result<(), _>::Ok(())` is 0.
    /// Return from the current call.
    Return,
    /// Self-destruct the current contract.
    SelfDestruct,

    // Revert Codes
    /// Revert the transaction.
    Revert = 0x10,
    /// Exceeded maximum call depth.
    CallTooDeep,
    /// Insufficient funds for transfer.
    OutOfFunds,
    /// Revert if `CREATE`/`CREATE2` starts with `0xEF00`.
    CreateInitCodeStartingEF00,
    /// Invalid EVM Object Format (EOF) init code.
    InvalidEOFInitCode,
    /// `ExtDelegateCall` calling a non EOF contract.
    InvalidExtDelegateCallTarget,

    // Halt Codes
    /// Out of gas error.
    OutOfGas = 0x20,
    /// Out of gas error encountered during memory expansion.
    MemoryOOG,
    /// The memory limit of the EVM has been exceeded.
    MemoryLimitOOG,
    /// Out of gas error encountered during the execution of a precompiled contract.
    PrecompileOOG,
    /// Out of gas error encountered while calling an invalid operand.
    InvalidOperandOOG,
    /// Out of gas error encountered while checking for reentrancy sentry.
    ReentrancySentryOOG,
    /// Invalid `CALL` with value transfer in static context.
    CallNotAllowedInsideStatic,
    /// Invalid state modification in static call.
    StateChangeDuringStaticCall,
    /// Invalid or undefined opcode.
    InvalidOpcode,
    /// Invalid jump destination. Dynamic jumps points to invalid not jumpdest opcode.
    InvalidJump,
    /// The feature or opcode is not activated in this version of the EVM.
    NotActivated,
    /// Attempting to pop a value from an empty stack.
    StackUnderflow,
    /// Attempting to push a value onto a full stack.
    StackOverflow,
    /// Invalid memory or storage offset.
    OutOfOffset,
    /// Address collision during contract creation.
    CreateCollision,
    /// Payment amount overflow.
    OverflowPayment,
    /// Error in precompiled contract execution.
    PrecompileError,
    /// Nonce overflow.
    NonceOverflow,
    /// Exceeded contract size limit during creation.
    CreateContractSizeLimit,
    /// Created contract starts with invalid bytes (`0xEF`).
    CreateContractStartingWithEF,
    /// Exceeded init code size limit (EIP-3860:  Limit and meter initcode).
    CreateInitCodeSizeLimit,
    /// Fatal precompile error.
    FatalPrecompileError,
    /// Fatal external error. Returned by database.
    FatalExternalError,
    /// Invalid encoding of an instruction's immediate operand.
    InvalidImmediateEncoding,
}

impl InstrStop {
    /// Returns whether execution completed successfully.
    #[doc(alias = "is_ok")]
    #[inline]
    pub const fn is_success(self) -> bool {
        matches!(self, Self::Stop | Self::Return | Self::SelfDestruct)
    }

    /// Returns whether execution reverted without an exceptional halt.
    #[inline]
    pub const fn is_revert(self) -> bool {
        matches!(
            self,
            Self::Revert
                | Self::CallTooDeep
                | Self::OutOfFunds
                | Self::CreateInitCodeStartingEF00
                | Self::InvalidEOFInitCode
                | Self::InvalidExtDelegateCallTarget
        )
    }

    /// Returns whether execution halted exceptionally.
    #[inline]
    pub const fn is_halt(self) -> bool {
        !self.is_success() && !self.is_revert()
    }

    /// Returns whether execution hit a fatal host/extension boundary error.
    #[inline]
    pub const fn is_fatal(self) -> bool {
        matches!(self, Self::FatalPrecompileError | Self::FatalExternalError)
    }
}
