//! Pushes the published DA limits to block builders over `miner_setMaxDASize`.

use std::time::Duration;

use alloy_primitives::U64;
use base_batcher_core::DaLimits;
use base_runtime::Runtime;
use jsonrpsee::{
    core::ClientError, http_client::HttpClient, proc_macros::rpc,
    types::error::METHOD_NOT_FOUND_CODE,
};
use tokio::sync::watch;
use tracing::{debug, warn};
use url::Url;

use crate::RpcClientBuilder;

/// Client-side jsonrpsee trait for the miner API extension.
#[rpc(client, namespace = "miner")]
trait MinerApiExt {
    /// Sets the maximum data availability size of any tx allowed in a block, and the total max l1
    /// data size of the block. 0 means no maximum.
    #[method(name = "setMaxDASize")]
    async fn set_max_da_size(&self, max_tx_size: U64, max_block_size: U64) -> RpcResult<bool>;
}

/// Pushes the published DA limits to one block builder.
///
/// The limits live in the block builder's memory, so a block builder that restarts loses them.
/// The pusher pushes them on every publication and every
/// [`REFRESH_INTERVAL`](Self::REFRESH_INTERVAL), which also retries a failed push and restores
/// the limits of a restarted block builder. A push outlasts neither the network timeout nor,
/// therefore, newer limits and shutdown by more than that.
///
/// A block builder that answers method not found does not serve the `miner` API. Retrying cannot
/// fix that and the batcher cannot throttle it, so [`run`](Self::run) returns the error and the
/// batcher stops. Any other failure is logged and retried.
#[derive(Debug)]
pub struct ThrottlePusher {
    client: HttpClient,
    /// The block builder origin, for logs and errors. It never carries a path or credentials.
    origin: String,
    limits: watch::Receiver<DaLimits>,
}

impl ThrottlePusher {
    /// How often the current limits are pushed while no new limits are published.
    pub const REFRESH_INTERVAL: Duration = Duration::from_secs(10);

    /// Creates a pusher of the limits published on `limits` to the block builder at `url`.
    ///
    /// # Errors
    ///
    /// Returns an error when `url` is not an HTTP URL.
    pub fn new(
        url: &Url,
        limits: watch::Receiver<DaLimits>,
        client_builder: RpcClientBuilder,
    ) -> eyre::Result<Self> {
        Ok(Self {
            client: client_builder.client(url)?,
            origin: url.origin().ascii_serialization(),
            limits,
        })
    }

    /// Pushes the limits until `runtime` is cancelled or the limits stop being published.
    ///
    /// # Errors
    ///
    /// Returns an error when the block builder does not serve `miner_setMaxDASize`.
    pub async fn run<R: Runtime>(mut self, runtime: R) -> eyre::Result<()> {
        loop {
            let limits = *self.limits.borrow_and_update();
            self.push(limits).await?;

            tokio::select! {
                biased;
                () = runtime.cancelled() => return Ok(()),
                changed = self.limits.changed() => {
                    if changed.is_err() {
                        return Ok(());
                    }
                }
                () = runtime.sleep(Self::REFRESH_INTERVAL) => {}
            }
        }
    }

    /// Pushes `limits` once. Only an error that retrying cannot fix is returned.
    async fn push(&self, limits: DaLimits) -> eyre::Result<()> {
        let result = self
            .client
            .set_max_da_size(U64::from(limits.max_tx_size), U64::from(limits.max_block_size))
            .await;

        match result {
            Ok(true) => {
                debug!(
                    block_builder = %self.origin,
                    max_tx_size = limits.max_tx_size,
                    max_block_size = limits.max_block_size,
                    "DA limits applied"
                );
            }
            Ok(false) => warn!(block_builder = %self.origin, "block builder refused the DA limits"),
            Err(ClientError::Call(error)) if error.code() == METHOD_NOT_FOUND_CODE => {
                eyre::bail!(
                    "block builder {} does not serve miner_setMaxDASize, required by the DA \
                     throttle unless --no-throttle is set: {error}",
                    self.origin
                );
            }
            Err(error) => {
                warn!(block_builder = %self.origin, error = %error, "failed to push DA limits");
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use base_runtime::{Cancellation, TokioRuntime};
    use httpmock::{Mock, prelude::*};

    use super::*;
    use crate::test_utils::rpc_client_builder;

    const LIMITS: DaLimits = DaLimits { max_tx_size: 150, max_block_size: 20_000 };

    /// A block builder that answers every request with `answer`, the `result` or `error` member
    /// of a JSON-RPC response.
    async fn block_builder(answer: &str) -> MockServer {
        let server = MockServer::start_async().await;
        let body = format!(r#"{{"jsonrpc":"2.0","id":0,{answer}}}"#);
        server
            .mock_async(|when, then| {
                when.method(POST).path("/");
                then.status(200).header("content-type", "application/json").body(body);
            })
            .await;
        server
    }

    /// Answers `true` to the requests whose JSON body includes `request`.
    async fn accept<'a>(server: &'a MockServer, request: &str) -> Mock<'a> {
        server
            .mock_async(|when, then| {
                when.method(POST).path("/").json_body_includes(request);
                then.status(200)
                    .header("content-type", "application/json")
                    .body(r#"{"jsonrpc":"2.0","id":0,"result":true}"#);
            })
            .await
    }

    /// A pusher to `server` that starts from [`LIMITS`], and the sender of its limits.
    fn pusher(server: &MockServer) -> (ThrottlePusher, watch::Sender<DaLimits>) {
        let (limits_tx, limits_rx) = watch::channel(LIMITS);
        let pusher =
            ThrottlePusher::new(&server.url("/").parse().unwrap(), limits_rx, rpc_client_builder())
                .unwrap();
        (pusher, limits_tx)
    }

    /// The limits are pushed as `miner_setMaxDASize` with the tx and block sizes as hex
    /// quantities.
    #[tokio::test]
    async fn push_encodes_the_limits_as_hex_quantities() {
        let server = MockServer::start_async().await;
        let request =
            accept(&server, r#"{"method":"miner_setMaxDASize","params":["0x96","0x4e20"]}"#).await;
        let (pusher, _limits_tx) = pusher(&server);

        pusher.push(LIMITS).await.unwrap();

        request.assert_async().await;
    }

    /// A block builder without the `miner` API is an error the batcher stops on, because it can
    /// never be throttled.
    #[tokio::test]
    async fn push_fails_when_the_block_builder_lacks_the_miner_api() {
        let server = block_builder(
            r#""error":{"code":-32601,"message":"the method miner_setMaxDASize does not exist"}"#,
        )
        .await;
        let (pusher, _limits_tx) = pusher(&server);

        assert!(pusher.push(LIMITS).await.is_err());
    }

    /// A refusal, an internal error and an unreachable block builder are not fatal.
    #[tokio::test]
    async fn push_does_not_fail_on_a_retryable_failure() {
        let refusing = block_builder(r#""result":false"#).await;
        let failing = block_builder(r#""error":{"code":-32603,"message":"internal error"}"#).await;
        let (to_refusing, _refusing_tx) = pusher(&refusing);
        let (to_failing, _failing_tx) = pusher(&failing);
        let (_unreachable_tx, unreachable_rx) = watch::channel(LIMITS);
        let to_unreachable = ThrottlePusher::new(
            &"http://127.0.0.1:1".parse().unwrap(),
            unreachable_rx,
            rpc_client_builder(),
        )
        .unwrap();

        to_refusing.push(LIMITS).await.unwrap();
        to_failing.push(LIMITS).await.unwrap();
        to_unreachable.push(LIMITS).await.unwrap();
    }

    /// The pusher pushes the current limits on start, then each new publication.
    #[tokio::test]
    async fn run_pushes_the_current_limits_then_every_publication() {
        let server = MockServer::start_async().await;
        let initial = accept(&server, r#"{"params":["0x96","0x4e20"]}"#).await;
        let updated = accept(&server, r#"{"params":["0x1","0x2"]}"#).await;
        let (pusher, limits_tx) = pusher(&server);
        let runtime = TokioRuntime::new();
        let task = tokio::spawn(pusher.run(runtime.clone()));

        while initial.calls_async().await == 0 {
            tokio::task::yield_now().await;
        }
        limits_tx.send_replace(DaLimits { max_tx_size: 1, max_block_size: 2 });
        while updated.calls_async().await == 0 {
            tokio::task::yield_now().await;
        }
        runtime.cancel();

        task.await.unwrap().unwrap();
    }
}
