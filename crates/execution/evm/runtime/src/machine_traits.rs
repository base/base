use auto_impl::auto_impl;
use base_execution_evm_machine::{ContextError, ContextTr, FrameStack};
use base_execution_evm_machine::{InterpreterResult, interpreter_action::FrameInit};
use base_execution_evm_runtime::EthInstructions;
use base_execution_evm_runtime::EvmMachine;

use crate::{
    ContextTrDbError, EthFrame, FrameResult, ItemOrResult, PrecompileProvider,
    item_or_result::FrameInitOrResult,
};

/// Type alias for database error within a context
pub type ContextDbError<CTX> = ContextError<ContextTrDbError<CTX>>;

/// Type alias for frame init result
pub type FrameInitResult<'a> = ItemOrResult<&'a mut EthFrame, FrameResult>;

/// A trait that integrates context, instruction set, and precompiles to create an EVM struct.
///
/// In addition to execution capabilities, this trait provides getter methods for its component fields.
#[auto_impl(&mut, Box)]
pub trait EvmTr {
    /// The context type that implements ContextTr to provide access to execution state
    type Context: ContextTr;

    /// The type containing the available precompiled contracts
    type Precompiles: PrecompileProvider<Self::Context>;

    /// Returns a tuple of references to the context, the frame and the instructions.
    #[expect(clippy::type_complexity)]
    fn all(
        &self,
    ) -> (&Self::Context, &EthInstructions<Self::Context>, &Self::Precompiles, &FrameStack<EthFrame>);

    /// Returns a tuple of mutable references to the context, the frame and the instructions.
    #[expect(clippy::type_complexity)]
    fn all_mut(
        &mut self,
    ) -> (
        &mut Self::Context,
        &mut EthInstructions<Self::Context>,
        &mut Self::Precompiles,
        &mut FrameStack<EthFrame>,
    );

    /// Returns a mutable reference to the execution context
    #[inline]
    fn ctx(&mut self) -> &mut Self::Context {
        let (ctx, _, _, _) = self.all_mut();
        ctx
    }

    /// Returns a mutable reference to the execution context
    #[inline]
    fn ctx_mut(&mut self) -> &mut Self::Context {
        self.ctx()
    }

    /// Returns an immutable reference to the execution context
    #[inline]
    fn ctx_ref(&self) -> &Self::Context {
        let (ctx, _, _, _) = self.all();
        ctx
    }

    /// Returns mutable references to both the context and instruction set.
    /// This enables atomic access to both components when needed.
    #[inline]
    fn ctx_instructions(&mut self) -> (&mut Self::Context, &mut EthInstructions<Self::Context>) {
        let (ctx, instructions, _, _) = self.all_mut();
        (ctx, instructions)
    }

    /// Returns mutable references to both the context and precompiles.
    /// This enables atomic access to both components when needed.
    #[inline]
    fn ctx_precompiles(&mut self) -> (&mut Self::Context, &mut Self::Precompiles) {
        let (ctx, _, precompiles, _) = self.all_mut();
        (ctx, precompiles)
    }

    /// Returns a mutable reference to the frame stack.
    #[inline]
    fn frame_stack(&mut self) -> &mut FrameStack<EthFrame> {
        let (_, _, _, frame_stack) = self.all_mut();
        frame_stack
    }

    /// Initializes the frame for the given frame input. Frame is pushed to the frame stack.
    fn frame_init(
        &mut self,
        frame_input: FrameInit,
    ) -> Result<FrameInitResult<'_>, ContextDbError<Self::Context>>;

    /// Run the frame from the top of the stack. Returns the frame init or result.
    ///
    /// If frame has returned result it would mark it as finished.
    fn frame_run(&mut self) -> Result<FrameInitOrResult, ContextDbError<Self::Context>>;

    /// Returns the result of the frame to the caller. Frame is popped from the frame stack.
    /// Consumes the frame result or returns it if there is more frames to run.
    fn frame_return_result(
        &mut self,
        result: FrameResult,
    ) -> Result<Option<FrameResult>, ContextDbError<Self::Context>>;
}

impl<CTX, INSP, P> EvmTr for EvmMachine<CTX, INSP, P>
where
    CTX: ContextTr,
    P: PrecompileProvider<CTX, Output = InterpreterResult>,
{
    type Context = CTX;

    type Precompiles = P;

    #[inline]
    fn all(
        &self,
    ) -> (&Self::Context, &EthInstructions<Self::Context>, &Self::Precompiles, &FrameStack<EthFrame>)
    {
        let ctx = &self.ctx;
        let instructions = &self.instruction;
        let precompiles = &self.precompiles;
        let frame_stack = &self.frame_stack;
        (ctx, instructions, precompiles, frame_stack)
    }

    #[inline]
    fn all_mut(
        &mut self,
    ) -> (
        &mut Self::Context,
        &mut EthInstructions<Self::Context>,
        &mut Self::Precompiles,
        &mut FrameStack<EthFrame>,
    ) {
        let ctx = &mut self.ctx;
        let instructions = &mut self.instruction;
        let precompiles = &mut self.precompiles;
        let frame_stack = &mut self.frame_stack;
        (ctx, instructions, precompiles, frame_stack)
    }

    /// Initializes the frame for the given frame input. Frame is pushed to the frame stack.
    #[inline]
    fn frame_init(
        &mut self,
        frame_input: FrameInit,
    ) -> Result<FrameInitResult<'_>, ContextDbError<CTX>> {
        let is_first_init = self.frame_stack.index().is_none();
        let new_frame =
            if is_first_init { self.frame_stack.start_init() } else { self.frame_stack.get_next() };

        let ctx = &mut self.ctx;
        let precompiles = &mut self.precompiles;
        let res = EthFrame::init_with_context(new_frame, ctx, precompiles, frame_input)?;

        Ok(res.map_item(|token| {
            if is_first_init {
                unsafe { self.frame_stack.end_init(token) };
            } else {
                unsafe { self.frame_stack.push(token) };
            }
            self.frame_stack.get()
        }))
    }

    /// Run the frame from the top of the stack. Returns the frame init or result.
    #[inline]
    fn frame_run(&mut self) -> Result<FrameInitOrResult, ContextDbError<CTX>> {
        let frame = self.frame_stack.get();
        let context = &mut self.ctx;
        let instructions = &mut self.instruction;

        let action = frame.interpreter.run_plain(
            instructions.instruction_table(),
            instructions.gas_table(),
            context,
        );

        frame.process_next_action(context, action).inspect(|i| {
            if i.is_result() {
                frame.set_finished(true);
            }
        })
    }

    /// Returns the result of the frame to the caller. Frame is popped from the frame stack.
    #[inline]
    fn frame_return_result(
        &mut self,
        result: FrameResult,
    ) -> Result<Option<FrameResult>, ContextDbError<Self::Context>> {
        if self.frame_stack.get().is_finished() {
            self.frame_stack.pop();
        }
        if self.frame_stack.index().is_none() {
            return Ok(Some(result));
        }
        self.frame_stack
            .get()
            .return_result::<_, ContextDbError<Self::Context>>(&mut self.ctx, result)?;
        Ok(None)
    }
}
