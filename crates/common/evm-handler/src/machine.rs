//! This module contains [`EvmMachine`] struct.
use core::{
    fmt::Debug,
    ops::{Deref, DerefMut},
};

use crate::EthInstructions;
use base_evm_context::FrameStack;

/// Main EVM structure that contains all data needed for execution.
#[derive(Debug)]
pub struct EvmMachine<CTX, INSP, P, F> {
    /// [`base_evm_context::ContextTr`] of the EVM it is used to fetch data from database.
    pub ctx: CTX,
    /// Inspector of the EVM it is used to inspect the EVM.
    /// Its trait are defined in revm-inspector crate.
    pub inspector: INSP,
    /// Opcode implementations and fork-specific gas costs.
    pub instruction: EthInstructions<CTX>,
    /// Precompile provider of the EVM it is used to execute precompiles.
    /// `PrecompileProvider` trait is defined in base-evm-handler crate.
    pub precompiles: P,
    /// Frame that is going to be executed.
    pub frame_stack: FrameStack<F>,
}

impl<CTX, P, F: Default> EvmMachine<CTX, (), P, F> {
    /// Create a new EVM instance with a given context, instruction set, and precompile provider.
    ///
    /// Inspector will be set to `()`.
    pub fn new(ctx: CTX, instruction: EthInstructions<CTX>, precompiles: P) -> Self {
        EvmMachine {
            ctx,
            inspector: (),
            instruction,
            precompiles,
            frame_stack: FrameStack::new_prealloc(8),
        }
    }
}

impl<CTX, INSP, P, F: Default> EvmMachine<CTX, INSP, P, F> {
    /// Create a new EVM instance with a given context, inspector, instruction set, and precompile provider.
    pub fn new_with_inspector(
        ctx: CTX,
        inspector: INSP,
        instruction: EthInstructions<CTX>,
        precompiles: P,
    ) -> Self {
        EvmMachine {
            ctx,
            inspector,
            instruction,
            precompiles,
            frame_stack: FrameStack::new_prealloc(8),
        }
    }
}

impl<CTX, INSP, P, F> EvmMachine<CTX, INSP, P, F> {
    /// Consumed self and returns new EvmMachine type with given Inspector.
    pub fn with_inspector<OINSP>(self, inspector: OINSP) -> EvmMachine<CTX, OINSP, P, F> {
        EvmMachine {
            ctx: self.ctx,
            inspector,

            instruction: self.instruction,
            precompiles: self.precompiles,
            frame_stack: self.frame_stack,
        }
    }

    /// Consumes self and returns new EvmMachine type with given Precompiles.
    pub fn with_precompiles<OP>(self, precompiles: OP) -> EvmMachine<CTX, INSP, OP, F> {
        EvmMachine {
            ctx: self.ctx,
            inspector: self.inspector,
            instruction: self.instruction,
            precompiles,
            frame_stack: self.frame_stack,
        }
    }

    /// Consumes self and returns inner Inspector.
    pub fn into_inspector(self) -> INSP {
        self.inspector
    }
}

impl<CTX, INSP, P, F> Deref for EvmMachine<CTX, INSP, P, F> {
    type Target = CTX;

    fn deref(&self) -> &Self::Target {
        &self.ctx
    }
}

impl<CTX, INSP, P, F> DerefMut for EvmMachine<CTX, INSP, P, F> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.ctx
    }
}
