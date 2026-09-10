use base_execution_evm_runtime::{
    CallOutcome, ContextDbError, ContextTr, EthFrame, EthInstructions, EvmTr, FrameInitOrResult,
    FrameInitResult, FrameResult, FrameStack, Inspector, ItemOrResult, JournalTr,
    inspect_instructions,
    inspector_handler::{frame_end, frame_start, inspect_logs},
    interpreter_action::FrameInit,
};

/// Inspector EVM trait. Extends the [`EvmTr`] trait with inspector related methods.
///
/// It contains execution of interpreter with [`crate::Inspector`] calls [`crate::Inspector::step`] and [`crate::Inspector::step_end`] calls.
///
/// It is used inside [`crate::InspectorHandler`] to extend evm with support for inspection.
pub trait InspectorEvmTr: EvmTr<Context: ContextTr> {
    /// The inspector type used for EVM execution inspection.
    type Inspector: Inspector<Self::Context>;

    /// Returns a tuple of mutable references to the context, the inspector, the frame and the instructions.
    ///
    /// This is one of two functions that need to be implemented for Evm. Second one is `all_mut`.
    #[expect(clippy::type_complexity)]
    fn all_inspector(
        &self,
    ) -> (
        &Self::Context,
        &EthInstructions<Self::Context>,
        &Self::Precompiles,
        &FrameStack<EthFrame>,
        &Self::Inspector,
    );

    /// Returns a tuple of mutable references to the context, the inspector, the frame and the instructions.
    ///
    /// This is one of two functions that need to be implemented for Evm. Second one is `all`.
    #[expect(clippy::type_complexity)]
    fn all_mut_inspector(
        &mut self,
    ) -> (
        &mut Self::Context,
        &mut EthInstructions<Self::Context>,
        &mut Self::Precompiles,
        &mut FrameStack<EthFrame>,
        &mut Self::Inspector,
    );

    /// Returns a mutable reference to the inspector.
    fn inspector(&mut self) -> &mut Self::Inspector {
        let (_, _, _, _, inspector) = self.all_mut_inspector();
        inspector
    }

    /// Returns a tuple of mutable references to the context and the inspector.
    ///
    /// Useful when you want to allow inspector to modify the context.
    fn ctx_inspector(&mut self) -> (&mut Self::Context, &mut Self::Inspector) {
        let (ctx, _, _, _, inspector) = self.all_mut_inspector();
        (ctx, inspector)
    }

    /// Returns a tuple of mutable references to the context, the inspector and the frame.
    ///
    /// Useful when you want to allow inspector to modify the context and the frame.
    fn ctx_inspector_frame(&mut self) -> (&mut Self::Context, &mut Self::Inspector, &mut EthFrame) {
        let (ctx, _, _, frame, inspector) = self.all_mut_inspector();
        (ctx, inspector, frame.get())
    }

    /// Returns a tuple of mutable references to the context, the inspector, the frame and the instructions.
    fn ctx_inspector_frame_instructions(
        &mut self,
    ) -> (
        &mut Self::Context,
        &mut Self::Inspector,
        &mut EthFrame,
        &mut EthInstructions<Self::Context>,
    ) {
        let (ctx, instructions, _, frame, inspector) = self.all_mut_inspector();
        (ctx, inspector, frame.get(), instructions)
    }

    /// Initializes the frame for the given frame input. Frame is pushed to the frame stack.
    #[inline]
    fn inspect_frame_init(
        &mut self,
        mut frame_init: FrameInit,
    ) -> Result<FrameInitResult<'_>, ContextDbError<Self::Context>> {
        let (ctx, inspector) = self.ctx_inspector();
        if let Some(mut output) = frame_start(ctx, inspector, &mut frame_init.frame_input) {
            frame_end(ctx, inspector, &frame_init.frame_input, &mut output);
            return Ok(ItemOrResult::Result(output));
        }

        let frame_input = frame_init.frame_input.clone();
        let logs_i = ctx.journal().logs().len();
        if let ItemOrResult::Result(mut output) = self.frame_init(frame_init)? {
            let (ctx, inspector) = self.ctx_inspector();
            // Logs journaled by the frame: the EIP-7708 transfer log, and the
            // logs of a precompile when one was called.
            if ctx.journal().logs().len() != logs_i {
                inspect_logs(None, ctx, inspector, logs_i);
            }
            // Custom precompiles gather their logs outside the journal.
            if let FrameResult::Call(CallOutcome {
                was_precompile_called: true,
                precompile_call_logs,
                ..
            }) = &output
            {
                for log in precompile_call_logs.clone() {
                    inspector.log(ctx, log);
                }
            }
            frame_end(ctx, inspector, &frame_input, &mut output);
            return Ok(ItemOrResult::Result(output));
        }

        // if it is new frame, initialize the interpreter.
        let (ctx, inspector, frame) = self.ctx_inspector_frame();
        if ctx.journal().logs().len() != logs_i {
            inspect_logs(None, ctx, inspector, logs_i);
        }
        inspector.initialize_interp(&mut frame.interpreter, ctx);
        Ok(ItemOrResult::Item(frame))
    }

    /// Run the frame from the top of the stack. Returns the frame init or result.
    ///
    /// If frame has returned result it would mark it as finished.
    #[inline]
    fn inspect_frame_run(&mut self) -> Result<FrameInitOrResult, ContextDbError<Self::Context>> {
        let (ctx, inspector, frame, instructions) = self.ctx_inspector_frame_instructions();

        let next_action = inspect_instructions(
            ctx,
            &mut frame.interpreter,
            inspector,
            instructions.instruction_table(),
            instructions.gas_table(),
        );
        let mut result = frame.process_next_action(ctx, next_action);

        if let Ok(ItemOrResult::Result(frame_result)) = &mut result {
            let (ctx, inspector, frame) = self.ctx_inspector_frame();
            // TODO When all_mut fn is added we can fetch inspector at the top of the function.s
            frame_end(ctx, inspector, &frame.input, frame_result);
            frame.set_finished(true);
        };
        result
    }
}
