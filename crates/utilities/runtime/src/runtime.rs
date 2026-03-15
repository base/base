//! Composite runtime supertrait combining Clock, Spawner, and Cancellation.

use crate::{Cancellation, Clock, Spawner};

/// A complete async runtime providing time, task spawning, and cancellation.
///
/// Components that need runtime capabilities accept a single `R: Runtime`
/// bound rather than three separate trait bounds. In production, use
/// [`TokioRuntime`]; in tests, use [`DeterministicRuntime`].
///
/// # Blanket implementation
///
/// Any type that implements [`Clock`] + [`Spawner`] + [`Cancellation`] +
/// [`Clone`] + [`Send`] + [`Sync`] + `'static` automatically implements
/// `Runtime`. No manual `impl Runtime for MyRuntime` is needed.
///
/// [`TokioRuntime`]: crate::TokioRuntime
/// [`DeterministicRuntime`]: crate::DeterministicRuntime
pub trait Runtime: Clock + Spawner + Cancellation + Clone + Send + Sync + 'static {}

impl<T> Runtime for T where T: Clock + Spawner + Cancellation + Clone + Send + Sync + 'static {}
