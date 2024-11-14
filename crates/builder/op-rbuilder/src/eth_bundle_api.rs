//! An implemention of the *internal* eth_sendBundle api used by rbuilder.
//!
//! Should be refactored into standalone crate if required by other code.

use jsonrpsee::{
    proc_macros::rpc,
    types::{ErrorCode, ErrorObjectOwned},
};
use rbuilder::{
    live_builder::order_input::rpc_server::RawCancelBundle,
    primitives::{
        serialize::{RawBundle, TxEncoding},
        Bundle,
    },
};
use tracing::warn;
use transaction_pool_bundle_ext::BundlePoolOperations;

/// [`EthBundleApiServer`] implementation.
pub struct EthBundleMinimalApi<BundlePool> {
    pool: BundlePool,
}

impl<BundlePool> EthBundleMinimalApi<BundlePool> {
    pub fn new(pool: BundlePool) -> Self {
        Self { pool }
    }
}

#[async_trait::async_trait]
impl<BundlePool> EthCallBundleMinimalApiServer for EthBundleMinimalApi<BundlePool>
where
    BundlePool:
        BundlePoolOperations<Bundle = Bundle, CancelBundleReq = RawCancelBundle> + Clone + 'static,
{
    fn send_bundle(&self, raw_bundle: RawBundle) -> jsonrpsee::core::RpcResult<()> {
        let bundle = match raw_bundle.try_into(TxEncoding::WithBlobData) {
            Ok(bundle) => bundle,
            Err(err) => {
                return Err(ErrorObjectOwned::owned(
                    ErrorCode::InvalidParams.code(),
                    format!("Failed to parse bundle: {:?}", err),
                    None::<()>,
                ));
            }
        };
        tokio::task::spawn_local({
            let pool = self.pool.clone();
            async move {
                if let Err(e) = pool.add_bundle(bundle).await {
                    warn!(?e, "Failed to send bundle");
                }
            }
        });
        Ok(())
    }

    fn cancel_bundle(&self, request: RawCancelBundle) -> jsonrpsee::core::RpcResult<()> {
        // Following rbuilder behavior to ignore errors
        tokio::task::spawn_local({
            let pool = self.pool.clone();
            async move {
                if let Err(e) = pool.cancel_bundle(request).await {
                    warn!(?e, "Failed to cancel bundle");
                }
            }
        });
        Ok(())
    }
}

/// A subset of the *internal* eth_sendBundle api used by rbuilder.
#[rpc(server, namespace = "eth")]
pub trait EthCallBundleMinimalApi {
    #[method(name = "sendBundle")]
    fn send_bundle(&self, bundle: RawBundle) -> jsonrpsee::core::RpcResult<()>;

    #[method(name = "cancelBundle")]
    fn cancel_bundle(&self, request: RawCancelBundle) -> jsonrpsee::core::RpcResult<()>;
}
