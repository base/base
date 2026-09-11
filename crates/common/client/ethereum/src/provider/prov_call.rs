use std::{
    future::Future,
    pin::Pin,
    task::{self, Poll},
};

use alloy_json_rpc::{RpcRecv, RpcSend};
use alloy_rpc_client::{RpcCall, Waiter};
use alloy_transport::TransportResult;
use futures::FutureExt;
use pin_project::pin_project;
use serde_json::value::RawValue;
use tokio::sync::oneshot;

#[cfg(not(target_family = "wasm"))]
/// Boxed future type used in [`ProviderCall`] for non-wasm targets.
pub type BoxedFut<Output> = Pin<Box<dyn Future<Output = TransportResult<Output>> + Send>>;

#[cfg(target_family = "wasm")]
/// Boxed future type used in [`ProviderCall`] for wasm targets.
pub type BoxedFut<Output> = Pin<Box<dyn Future<Output = TransportResult<Output>>>>;
/// The primary future type for the [`Provider`].
///
/// This future abstracts over several potential data sources. It allows
/// providers to:
/// - produce data via an [`RpcCall`]
/// - produce data by waiting on a batched RPC [`Waiter`]
/// - proudce data via an arbitrary boxed future
/// - produce data in any synchronous way
///
/// [`Provider`]: base_common_client_ethereum::Provider
#[pin_project(project = ProviderCallProj)]
pub enum ProviderCall<Params, Resp, Output = Resp, Map = fn(Resp) -> Output>
where
    Params: RpcSend,
    Resp: RpcRecv,
    Map: Fn(Resp) -> Output,
{
    /// An underlying call to an RPC server.
    RpcCall(RpcCall<Params, Resp, Output, Map>),
    /// A waiter for a batched call to a remote RPC server.
    Waiter(Waiter<Resp, Output, Map>),
    /// A boxed future.
    BoxedFuture(BoxedFut<Output>),
    /// The output, produces synchronously.
    Ready(Option<TransportResult<Output>>),
}

impl<Params, Resp, Output, Map> ProviderCall<Params, Resp, Output, Map>
where
    Params: RpcSend,
    Resp: RpcRecv,
    Map: Fn(Resp) -> Output,
{
    /// Instantiate a new [`ProviderCall`] from the output.
    pub const fn ready(output: TransportResult<Output>) -> Self {
        Self::Ready(Some(output))
    }

    /// True if this is a ready value.
    pub const fn is_ready(&self) -> bool {
        matches!(self, Self::Ready(_))
    }

    /// Set a function to map the response into a different type. This is
    /// useful for transforming the response into a more usable type, e.g.
    /// changing `U64` to `u64`.
    ///
    /// This function fails if the inner future is not an [`RpcCall`] or
    /// [`Waiter`].
    ///
    /// ## Note
    ///
    /// Carefully review the rust documentation on [fn pointers] before passing
    /// them to this function. Unless the pointer is specifically coerced to a
    /// `fn(_) -> _`, the `NewMap` will be inferred as that function's unique
    /// type. This can lead to confusing error messages.
    ///
    /// [fn pointers]: https://doc.rust-lang.org/std/primitive.fn.html#creating-function-pointers
    pub fn map_resp<NewOutput, NewMap>(
        self,
        map: NewMap,
    ) -> Result<ProviderCall<Params, Resp, NewOutput, NewMap>, Self>
    where
        NewMap: Fn(Resp) -> NewOutput + Clone,
    {
        match self {
            Self::RpcCall(call) => Ok(ProviderCall::RpcCall(call.map_resp(map))),
            Self::Waiter(waiter) => Ok(ProviderCall::Waiter(waiter.map_resp(map))),
            _ => Err(self),
        }
    }
}

impl<Params, Resp, Output, Map> ProviderCall<&Params, Resp, Output, Map>
where
    Params: RpcSend + ToOwned,
    Params::Owned: RpcSend,
    Resp: RpcRecv,
    Map: Fn(Resp) -> Output,
{
    /// Convert this call into one with owned params, by cloning the params.
    ///
    /// # Panics
    ///
    /// Panics if called after the request has been polled.
    pub fn into_owned_params(self) -> ProviderCall<Params::Owned, Resp, Output, Map> {
        match self {
            Self::RpcCall(call) => ProviderCall::RpcCall(call.into_owned_params()),
            _ => panic!(),
        }
    }
}

impl<Params, Resp> std::fmt::Debug for ProviderCall<Params, Resp>
where
    Params: RpcSend,
    Resp: RpcRecv,
{
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::RpcCall(call) => f.debug_tuple("RpcCall").field(call).finish(),
            Self::Waiter { .. } => f.debug_struct("Waiter").finish_non_exhaustive(),
            Self::BoxedFuture(_) => f.debug_struct("BoxedFuture").finish_non_exhaustive(),
            Self::Ready(_) => f.debug_struct("Ready").finish_non_exhaustive(),
        }
    }
}

impl<Params, Resp, Output, Map> From<RpcCall<Params, Resp, Output, Map>>
    for ProviderCall<Params, Resp, Output, Map>
where
    Params: RpcSend,
    Resp: RpcRecv,
    Map: Fn(Resp) -> Output,
{
    fn from(call: RpcCall<Params, Resp, Output, Map>) -> Self {
        Self::RpcCall(call)
    }
}

impl<Params, Resp> From<Waiter<Resp>> for ProviderCall<Params, Resp, Resp, fn(Resp) -> Resp>
where
    Params: RpcSend,
    Resp: RpcRecv,
{
    fn from(waiter: Waiter<Resp>) -> Self {
        Self::Waiter(waiter)
    }
}

impl<Params, Resp, Output, Map> From<BoxedFut<Output>> for ProviderCall<Params, Resp, Output, Map>
where
    Params: RpcSend,
    Resp: RpcRecv,
    Map: Fn(Resp) -> Output,
{
    fn from(fut: BoxedFut<Output>) -> Self {
        Self::BoxedFuture(fut)
    }
}

impl<Params, Resp> From<oneshot::Receiver<TransportResult<Box<RawValue>>>>
    for ProviderCall<Params, Resp>
where
    Params: RpcSend,
    Resp: RpcRecv,
{
    fn from(rx: oneshot::Receiver<TransportResult<Box<RawValue>>>) -> Self {
        Waiter::from(rx).into()
    }
}

impl<Params, Resp, Output, Map> Future for ProviderCall<Params, Resp, Output, Map>
where
    Params: RpcSend,
    Resp: RpcRecv,
    Output: 'static,
    Map: Fn(Resp) -> Output,
{
    type Output = TransportResult<Output>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut task::Context<'_>) -> task::Poll<Self::Output> {
        match self.as_mut().project() {
            ProviderCallProj::RpcCall(call) => call.poll_unpin(cx),
            ProviderCallProj::Waiter(waiter) => waiter.poll_unpin(cx),
            ProviderCallProj::BoxedFuture(fut) => fut.poll_unpin(cx),
            ProviderCallProj::Ready(output) => {
                Poll::Ready(output.take().expect("output taken twice"))
            }
        }
    }
}
