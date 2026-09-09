//! Composite runtime supertrait combining Clock, Spawner, and Cancellation.

use crate::{Cancellation, Clock, Spawner};

/// A complete async runtime providing time, task spawning, and cancellation.
///
/// Components that need runtime capabilities accept a single `R: AsyncRuntime`
/// bound rather than three separate trait bounds. In production, use
/// `TokioRuntime`; in tests, use `Runner::start` with `Context`.
///
/// # Blanket implementation
///
/// Any type that implements [`Clock`] + [`Spawner`] + [`Cancellation`] +
/// [`Clone`] + [`Send`] + [`Sync`] + `'static` automatically implements
/// `AsyncRuntime`. No manual `impl AsyncRuntime for MyRuntime` is needed.
pub trait AsyncRuntime: Clock + Spawner + Cancellation + Clone + Send + Sync + 'static {}

impl<T> AsyncRuntime for T where T: Clock + Spawner + Cancellation + Clone + Send + Sync + 'static {}
