use std::{future::IntoFuture, marker::PhantomData, time::Duration};

use alloy_dyn_abi::{DynSolValue, FunctionExt};
use alloy_json_abi::Function;
use alloy_primitives::{Address, Bytes};
use alloy_sol_types::SolCall;
use base_common_types_rpc::{
    BlockId, BlockOverrides,
    state::{AccountOverride, StateOverride},
};
#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
use tokio::time::{Timeout, timeout as timeout_future};
#[cfg(all(target_family = "wasm", target_os = "unknown"))]
use wasmtimer::tokio::{Timeout, timeout as timeout_future};

use crate::{
    Network,
    contracts::{Error, Result},
};

/// Raw coder.
const RAW_CODER: () = ();

#[expect(unnameable_types)]
mod private {
    pub trait Sealed {}
    impl Sealed for super::Function {}
    impl<C: super::SolCall> Sealed for super::PhantomData<C> {}
    impl Sealed for () {}
}

/// An [`crate::EthCall`] with an abi decoder.
#[must_use = "EthCall must be awaited to execute the call"]
#[derive(Clone, Debug)]
pub struct EthCall<'coder, D, N>
where
    N: Network,
    D: CallDecoder,
{
    inner: crate::EthCall<N, Bytes>,

    decoder: &'coder D,
}

impl<'coder, D, N> EthCall<'coder, D, N>
where
    N: Network,
    D: CallDecoder,
{
    /// Create a new [`EthCall`].
    pub const fn new(inner: crate::EthCall<N, Bytes>, decoder: &'coder D) -> Self {
        Self { inner, decoder }
    }
}

impl<N> EthCall<'static, (), N>
where
    N: Network,
{
    /// Create a new [`EthCall`].
    pub const fn new_raw(inner: crate::EthCall<N, Bytes>) -> Self {
        Self::new(inner, &RAW_CODER)
    }
}

impl<D, N> EthCall<'_, D, N>
where
    N: Network,
    D: CallDecoder,
{
    /// Swap the decoder for this call.
    pub fn with_decoder<E>(self, decoder: &E) -> EthCall<'_, E, N>
    where
        E: CallDecoder,
    {
        EthCall { inner: self.inner, decoder }
    }

    /// Wraps this call in a client-side timeout that only stops waiting for the response.
    ///
    /// Awaiting the returned future produces a timeout result around the existing contract result,
    /// so the two error cases can be handled separately.
    ///
    /// ```no_run
    /// # async fn example<P: crate::Provider>(
    /// #     provider: P,
    /// # ) -> Result<(), Box<dyn std::error::Error>> {
    /// use alloy_primitives::Address;
    /// use alloy_sol_types::sol;
    /// use std::time::Duration;
    ///
    /// sol! {
    ///     #![sol(alloy_contract = base_common_client_ethereum)]
    ///     #[sol(rpc)]
    ///     contract Token {
    ///         function balanceOf(address owner) external view returns (uint256);
    ///     }
    /// }
    ///
    /// let contract = Token::new(Address::ZERO, &provider);
    /// let call = contract.balanceOf(Address::ZERO);
    /// let balance = call.call().timeout(Duration::from_secs(10)).await??;
    /// # let _ = balance;
    /// # Ok(())
    /// # }
    /// ```
    ///
    /// # Panics
    ///
    /// On Tokio-backed targets, including WASI, the returned future panics when polled if there
    /// is no current Tokio timer, for example when polled outside of a Tokio runtime.
    pub fn timeout(self, duration: Duration) -> Timeout<<Self as IntoFuture>::IntoFuture> {
        timeout_future(duration, self.into_future())
    }

    /// Set the state overrides for this call.
    pub fn overrides(mut self, overrides: impl Into<StateOverride>) -> Self {
        self.inner = self.inner.overrides(overrides);
        self
    }

    /// Appends a single [AccountOverride] to the state override.
    ///
    /// Creates a new [`StateOverride`] if none has been set yet.
    pub fn account_override(
        mut self,
        address: Address,
        account_overrides: AccountOverride,
    ) -> Self {
        self.inner = self.inner.account_override(address, account_overrides);
        self
    }
    /// Extends the given [AccountOverride] to the state override.
    ///
    /// Creates a new [`StateOverride`] if none has been set yet.
    pub fn account_overrides(
        mut self,
        overrides: impl IntoIterator<Item = (Address, AccountOverride)>,
    ) -> Self {
        self.inner = self.inner.account_overrides(overrides);
        self
    }

    /// Set the block to use for this call.
    pub fn block(mut self, block: BlockId) -> Self {
        self.inner = self.inner.block(block);
        self
    }

    /// Sets the block overrides for this call.
    pub fn with_block_overrides(mut self, overrides: BlockOverrides) -> Self {
        self.inner = self.inner.with_block_overrides(overrides);
        self
    }
}

impl<N> From<crate::EthCall<N, Bytes>> for EthCall<'static, (), N>
where
    N: Network,
{
    fn from(inner: crate::EthCall<N, Bytes>) -> Self {
        Self { inner, decoder: &RAW_CODER }
    }
}

impl<'coder, D, N> std::future::IntoFuture for EthCall<'coder, D, N>
where
    D: CallDecoder,
    N: Network,
{
    type Output = Result<D::CallOutput>;

    type IntoFuture = EthCallFut<'coder, D, N>;

    fn into_future(self) -> Self::IntoFuture {
        EthCallFut { inner: self.inner.into_future(), decoder: self.decoder }
    }
}

/// Future for the [`EthCall`] type. This future wraps an RPC call with an abi
/// decoder.
#[must_use = "futures do nothing unless you `.await` or poll them"]
#[derive(Debug)]
#[expect(unnameable_types)]
pub struct EthCallFut<'coder, D, N>
where
    N: Network,
    D: CallDecoder,
{
    inner: <crate::EthCall<N, Bytes> as IntoFuture>::IntoFuture,
    decoder: &'coder D,
}

impl<D, N> std::future::Future for EthCallFut<'_, D, N>
where
    D: CallDecoder,
    N: Network,
{
    type Output = Result<D::CallOutput>;

    fn poll(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Self::Output> {
        let this = self.get_mut();
        let pin = std::pin::pin!(&mut this.inner);
        match pin.poll(cx) {
            std::task::Poll::Ready(Ok(data)) => {
                std::task::Poll::Ready(this.decoder.abi_decode_output(data))
            }
            std::task::Poll::Ready(Err(e)) => std::task::Poll::Ready(Err(e.into())),
            std::task::Poll::Pending => std::task::Poll::Pending,
        }
    }
}

/// A trait for decoding the output of a contract function.
///
/// This trait is sealed and cannot be implemented manually.
/// It is an implementation detail of [`CallBuilder`].
///
/// [`CallBuilder`]: crate::contracts::CallBuilder
pub trait CallDecoder: private::Sealed {
    // Not public API.

    /// The output type of the contract function.
    #[doc(hidden)]
    type CallOutput;

    /// Decodes the output of a contract function.
    #[doc(hidden)]
    fn abi_decode_output(&self, data: Bytes) -> Result<Self::CallOutput>;

    #[doc(hidden)]
    fn as_debug_field(&self) -> impl std::fmt::Debug;
}

impl CallDecoder for Function {
    type CallOutput = Vec<DynSolValue>;

    #[inline]
    fn abi_decode_output(&self, data: Bytes) -> Result<Self::CallOutput> {
        FunctionExt::abi_decode_output(self, &data).map_err(|e| Error::decode(&self.name, &data, e))
    }

    #[inline]
    fn as_debug_field(&self) -> impl std::fmt::Debug {
        self
    }
}

impl<C: SolCall> CallDecoder for PhantomData<C> {
    type CallOutput = C::Return;

    #[inline]
    fn abi_decode_output(&self, data: Bytes) -> Result<Self::CallOutput> {
        C::abi_decode_returns(&data).map_err(|e| Error::decode(C::SIGNATURE, &data, e.into()))
    }

    #[inline]
    fn as_debug_field(&self) -> impl std::fmt::Debug {
        std::any::type_name::<C>()
    }
}

impl CallDecoder for () {
    type CallOutput = Bytes;

    #[inline]
    fn abi_decode_output(&self, data: Bytes) -> Result<Self::CallOutput> {
        Ok(data)
    }

    #[inline]
    fn as_debug_field(&self) -> impl std::fmt::Debug {
        format_args!("()")
    }
}
