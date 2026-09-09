//! Types for launching execution extensions (ExEx).

use std::future::Future;

use futures::{FutureExt, future::BoxFuture};
use reth_db_api::{Database, database_metrics::DatabaseMetrics};
use reth_exex::ExExContext;

/// A trait for launching an `ExEx`.
pub trait LaunchExEx<DB: Database + DatabaseMetrics + Clone + Unpin + 'static>: Send {
    /// Launches the `ExEx`.
    ///
    /// The `ExEx` should be able to run independently and emit events on the channels provided in
    /// the [`ExExContext`].
    fn launch(
        self,
        ctx: ExExContext<DB>,
    ) -> impl Future<Output = eyre::Result<impl Future<Output = eyre::Result<()>> + Send>> + Send;
}

/// A boxed exex future.
pub type BoxExEx = BoxFuture<'static, eyre::Result<()>>;

/// A version of [`LaunchExEx`] that returns a boxed future. Makes the trait object-safe.
pub trait BoxedLaunchExEx<DB: Database + DatabaseMetrics + Clone + Unpin + 'static>: Send {
    /// Launches the `ExEx` and returns a boxed future.
    fn launch(self: Box<Self>, ctx: ExExContext<DB>) -> BoxFuture<'static, eyre::Result<BoxExEx>>;
}

/// Implements [`BoxedLaunchExEx`] for any [`LaunchExEx`] that is [Send] and `'static`.
///
/// Returns a [`BoxFuture`] that resolves to a [`BoxExEx`].
impl<E, DB: Database + DatabaseMetrics + Clone + Unpin + 'static> BoxedLaunchExEx<DB> for E
where
    E: LaunchExEx<DB> + Send + 'static,
{
    fn launch(self: Box<Self>, ctx: ExExContext<DB>) -> BoxFuture<'static, eyre::Result<BoxExEx>> {
        async move {
            let exex = LaunchExEx::launch(*self, ctx).await?;
            Ok(Box::pin(exex) as BoxExEx)
        }
        .boxed()
    }
}

/// Implements `LaunchExEx` for any closure that takes an [`ExExContext`] and returns a future
/// resolving to an `ExEx`.
impl<DB: Database + DatabaseMetrics + Clone + Unpin + 'static, F, Fut, E> LaunchExEx<DB> for F
where
    F: FnOnce(ExExContext<DB>) -> Fut + Send,
    Fut: Future<Output = eyre::Result<E>> + Send,
    E: Future<Output = eyre::Result<()>> + Send,
{
    fn launch(
        self,
        ctx: ExExContext<DB>,
    ) -> impl Future<Output = eyre::Result<impl Future<Output = eyre::Result<()>> + Send>> + Send
    {
        self(ctx)
    }
}
