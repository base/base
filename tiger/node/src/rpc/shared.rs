//! An inspector the RPC keeps a handle to while an EVM borrowing the node's state runs it.
//!
//! evm2 hands an installed inspector back by type only for `'static` EVMs, and RPC EVMs borrow a
//! short-lived `NodeState`, so the inspector lives behind a shared cell instead.

use std::{cell::RefCell, rc::Rc};

use alloy_primitives::{Address, Log, U256};
use evm2::{
    EvmTypesHost,
    Inspector,
    interpreter::{Interpreter, Message, MessageResult},
};

/// An inspector shared between an EVM and the RPC method that reads its result.
#[derive(Debug, Default)]
pub(super) struct Shared<I>(pub(super) Rc<RefCell<I>>);

impl<I> Shared<I> {
    /// Wraps `inspector`.
    pub(super) fn new(inspector: I) -> Self {
        Self(Rc::new(RefCell::new(inspector)))
    }

    /// Another handle to the same inspector, to install on an EVM.
    pub(super) fn handle(&self) -> Self {
        Self(Rc::clone(&self.0))
    }

    /// The inspector, once every other handle (the EVM's) is gone.
    pub(super) fn into_inner(self) -> I {
        match Rc::try_unwrap(self.0) {
            Ok(cell) => cell.into_inner(),
            Err(_) => panic!("the EVM still holds the inspector"),
        }
    }
}

impl<T: EvmTypesHost, I: Inspector<T>> Inspector<T> for Shared<I> {
    fn initialize_interp(&mut self, interp: &mut Interpreter<'_, '_, T>) {
        self.0.borrow_mut().initialize_interp(interp);
    }

    fn step(&mut self, interp: &mut Interpreter<'_, '_, T>) {
        self.0.borrow_mut().step(interp);
    }

    fn step_end(&mut self, interp: &mut Interpreter<'_, '_, T>) {
        self.0.borrow_mut().step_end(interp);
    }

    fn log(&mut self, log: &Log, host: &mut T::Host<'_>) {
        self.0.borrow_mut().log(log, host);
    }

    fn call(&mut self, interp: &mut Interpreter<'_, '_, T>, message: &mut Message<T>) -> Option<MessageResult<T>> {
        self.0.borrow_mut().call(interp, message)
    }

    fn call_end(&mut self, interp: &mut Interpreter<'_, '_, T>, message: &Message<T>, result: &mut MessageResult<T>) {
        self.0.borrow_mut().call_end(interp, message, result);
    }

    fn create(&mut self, interp: &mut Interpreter<'_, '_, T>, message: &mut Message<T>) -> Option<MessageResult<T>> {
        self.0.borrow_mut().create(interp, message)
    }

    fn create_end(&mut self, interp: &mut Interpreter<'_, '_, T>, message: &Message<T>, result: &mut MessageResult<T>) {
        self.0.borrow_mut().create_end(interp, message, result);
    }

    fn selfdestruct(&mut self, contract: &Address, target: &Address, value: &U256, host: &mut T::Host<'_>) {
        self.0.borrow_mut().selfdestruct(contract, target, value, host);
    }
}

/// The opcode and remaining gas of the last instruction stepped.
#[derive(Debug, Default)]
pub(super) struct LastStep(pub(super) Option<(u8, u64)>);

impl<T: EvmTypesHost> Inspector<T> for LastStep {
    fn step(&mut self, interp: &mut Interpreter<'_, '_, T>) {
        self.0 = Some((interp.opcode(), interp.gas().remaining()));
    }
}
