//! Types for launching execution extensions (ExEx).

use std::future::Future;

use futures::{FutureExt, future::BoxFuture};
use reth_exex::ExExContext;

/// A trait for launching an `ExEx`.
pub trait LaunchExEx: Send {
    /// Launches the `ExEx`.
    ///
    /// The `ExEx` should be able to run independently and emit events on the channels provided in
    /// the [`ExExContext`].
    fn launch(
        self,
        ctx: ExExContext,
    ) -> impl Future<Output = eyre::Result<impl Future<Output = eyre::Result<()>> + Send>> + Send;
}

/// A boxed exex future.
pub type BoxExEx = BoxFuture<'static, eyre::Result<()>>;

/// A version of [`LaunchExEx`] that returns a boxed future. Makes the trait object-safe.
pub trait BoxedLaunchExEx: Send {
    /// Launches the `ExEx` and returns a boxed future.
    fn launch(self: Box<Self>, ctx: ExExContext) -> BoxFuture<'static, eyre::Result<BoxExEx>>;
}

/// Implements [`BoxedLaunchExEx`] for any [`LaunchExEx`] that is [Send] and `'static`.
///
/// Returns a [`BoxFuture`] that resolves to a [`BoxExEx`].
impl<E> BoxedLaunchExEx for E
where
    E: LaunchExEx + Send + 'static,
{
    fn launch(self: Box<Self>, ctx: ExExContext) -> BoxFuture<'static, eyre::Result<BoxExEx>> {
        async move {
            let exex = LaunchExEx::launch(*self, ctx).await?;
            Ok(Box::pin(exex) as BoxExEx)
        }
        .boxed()
    }
}

/// Implements `LaunchExEx` for any closure that takes an [`ExExContext`] and returns a future
/// resolving to an `ExEx`.
impl<F, Fut, E> LaunchExEx for F
where
    F: FnOnce(ExExContext) -> Fut + Send,
    Fut: Future<Output = eyre::Result<E>> + Send,
    E: Future<Output = eyre::Result<()>> + Send,
{
    fn launch(
        self,
        ctx: ExExContext,
    ) -> impl Future<Output = eyre::Result<impl Future<Output = eyre::Result<()>> + Send>> + Send
    {
        self(ctx)
    }
}
