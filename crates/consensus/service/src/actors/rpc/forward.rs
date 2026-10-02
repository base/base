//! RPC middleware that forwards methods the consensus server does not serve to an upstream server.
//!
//! The unified `base` binary embeds an execution node next to the consensus node. Installing this
//! middleware lets the consensus RPC endpoint answer both the consensus namespaces it registers
//! itself and every other method by proxying it to the execution node's HTTP server.

use std::{collections::HashSet, sync::Arc, time::Duration};

use jsonrpsee::{
    RpcModule,
    core::{
        ClientError, JsonRawValue,
        client::ClientT,
        middleware::{Batch, BatchEntry, Notification, RpcServiceT},
        server::{BatchResponseBuilder, MethodResponse, ResponsePayload},
        traits::ToRpcParams,
    },
    http_client::{HttpClient, HttpClientBuilder},
    types::{ErrorCode, ErrorObject, ErrorObjectOwned, Request},
};
use tower::Layer;
use url::Url;

/// Upper bound for a proxied response, matching the jsonrpsee server default.
const MAX_RESPONSE_SIZE: usize = 10 * 1024 * 1024;

/// Raw, already-serialized request params.
struct RawParams(Option<Box<JsonRawValue>>);

impl ToRpcParams for RawParams {
    fn to_rpc_params(self) -> Result<Option<Box<JsonRawValue>>, serde_json::Error> {
        Ok(self.0)
    }
}

/// Shared state for forwarding requests to the upstream server.
#[derive(Debug)]
struct Upstream {
    client: HttpClient,
    /// Names of the methods served locally, which are never forwarded.
    local_methods: HashSet<&'static str>,
}

/// A [`Layer`] that installs [`ForwardUnmatched`].
#[derive(Debug, Clone)]
pub struct ForwardUnmatchedLayer {
    upstream: Arc<Upstream>,
}

impl ForwardUnmatchedLayer {
    /// Creates a layer forwarding every method not registered on `module` to `upstream`.
    pub(crate) fn new(
        upstream: &Url,
        module: &RpcModule<()>,
        timeout: Duration,
    ) -> Result<Self, ClientError> {
        let client = HttpClientBuilder::default().request_timeout(timeout).build(upstream)?;
        let local_methods = module.method_names().collect();
        Ok(Self { upstream: Arc::new(Upstream { client, local_methods }) })
    }
}

impl<S> Layer<S> for ForwardUnmatchedLayer {
    type Service = ForwardUnmatched<S>;

    fn layer(&self, service: S) -> Self::Service {
        ForwardUnmatched { service, upstream: Arc::clone(&self.upstream) }
    }
}

/// [`RpcServiceT`] that proxies unknown methods to the upstream server.
#[derive(Debug, Clone)]
pub struct ForwardUnmatched<S> {
    service: S,
    upstream: Arc<Upstream>,
}

impl<S> ForwardUnmatched<S>
where
    S: RpcServiceT<MethodResponse = MethodResponse> + Clone + Send + Sync + 'static,
{
    fn call_owned<'a>(&self, req: Request<'a>) -> impl Future<Output = MethodResponse> + Send + 'a {
        let service = self.service.clone();
        let upstream = Arc::clone(&self.upstream);
        async move {
            if upstream.local_methods.contains(req.method.as_ref()) {
                return service.call(req).await;
            }

            let Request { id, method, params, extensions, .. } = req;
            let params = RawParams(params.map(std::borrow::Cow::into_owned));
            let payload =
                match upstream.client.request::<Box<JsonRawValue>, _>(&method, params).await {
                    Ok(result) => ResponsePayload::success(result),
                    Err(ClientError::Call(err)) => ResponsePayload::<Box<JsonRawValue>>::error(err),
                    Err(err) => {
                        warn!(target: "rpc", %method, %err, "failed to forward request upstream");
                        ResponsePayload::error(ErrorObjectOwned::from(ErrorObject::from(
                            ErrorCode::InternalError,
                        )))
                    }
                };
            MethodResponse::response(id, payload, MAX_RESPONSE_SIZE).with_extensions(extensions)
        }
    }
}

impl<S> RpcServiceT for ForwardUnmatched<S>
where
    S: RpcServiceT<
            MethodResponse = MethodResponse,
            BatchResponse = MethodResponse,
            NotificationResponse = MethodResponse,
        > + Clone
        + Send
        + Sync
        + 'static,
{
    type MethodResponse = MethodResponse;
    type NotificationResponse = MethodResponse;
    type BatchResponse = MethodResponse;

    fn call<'a>(&self, req: Request<'a>) -> impl Future<Output = Self::MethodResponse> + Send + 'a {
        self.call_owned(req)
    }

    fn batch<'a>(&self, batch: Batch<'a>) -> impl Future<Output = Self::BatchResponse> + Send + 'a {
        // The inner service's `batch` dispatches to its own `call`, which would bypass this
        // middleware, so every entry is routed through `self.call` instead.
        let this = self.clone();
        async move {
            let mut responses = BatchResponseBuilder::new_with_limit(MAX_RESPONSE_SIZE);
            let mut got_notification = false;

            for entry in batch {
                let response = match entry {
                    Ok(BatchEntry::Call(req)) => this.call(req).await,
                    Ok(BatchEntry::Notification(n)) => {
                        got_notification = true;
                        this.service.notification(n).await;
                        continue;
                    }
                    Err(err) => {
                        let (err, id) = err.into_parts();
                        MethodResponse::error(id, err)
                    }
                };
                if let Err(too_big) = responses.append(response) {
                    return too_big;
                }
            }

            if responses.is_empty() && got_notification {
                MethodResponse::notification()
            } else {
                MethodResponse::from_batch(responses.finish())
            }
        }
    }

    fn notification<'a>(
        &self,
        n: Notification<'a>,
    ) -> impl Future<Output = Self::NotificationResponse> + Send + 'a {
        self.service.notification(n)
    }
}

#[cfg(test)]
mod tests {
    use std::{net::SocketAddr, num::NonZeroUsize};

    use jsonrpsee::{
        core::{client::ClientT, params::BatchRequestBuilder},
        http_client::HttpClientBuilder,
        rpc_params,
        server::{Server, ServerHandle},
    };

    use super::*;
    use crate::actors::rpc::actor::bind_rpc_server;

    async fn upstream_server() -> (SocketAddr, ServerHandle) {
        let server = Server::builder().build("127.0.0.1:0").await.unwrap();
        let mut module = RpcModule::new(());
        module.register_method("eth_chainId", |_, _, _| "0x2105").unwrap();
        module.register_method("eth_shadowed", |_, _, _| "upstream").unwrap();
        module
            .register_method("eth_fail", |_, _, _| {
                Err::<(), _>(ErrorObjectOwned::owned(3, "upstream failure", None::<()>))
            })
            .unwrap();
        let addr = server.local_addr().unwrap();
        (addr, server.start(module))
    }

    async fn front_server(upstream: SocketAddr) -> (SocketAddr, ServerHandle) {
        let mut module = RpcModule::new(());
        module.register_method("optimism_syncStatus", |_, _, _| "local").unwrap();
        module.register_method("eth_shadowed", |_, _, _| "local").unwrap();
        let config = base_consensus_rpc::RpcBuilder {
            socket: "127.0.0.1:0".parse().unwrap(),
            no_restart: true,
            enable_admin: false,
            admin_persistence: None,
            ws_enabled: false,
            dev_enabled: false,
            http_timeout: Duration::from_secs(5),
            max_concurrent_requests: NonZeroUsize::new(16).expect("nonzero"),
            forward_unmatched_to: Some(Url::parse(&format!("http://{upstream}")).unwrap()),
        };
        bind_rpc_server(&config, module).await.unwrap()
    }

    #[tokio::test]
    async fn forwards_unmatched_methods_and_keeps_local_ones() {
        let (upstream_addr, _upstream) = upstream_server().await;
        let (front_addr, _front) = front_server(upstream_addr).await;
        let client = HttpClientBuilder::default().build(format!("http://{front_addr}")).unwrap();

        let local: String = client.request("optimism_syncStatus", rpc_params![]).await.unwrap();
        assert_eq!(local, "local");

        let forwarded: String = client.request("eth_chainId", rpc_params![]).await.unwrap();
        assert_eq!(forwarded, "0x2105");

        // A method registered locally is never forwarded, even if the upstream also serves it.
        let shadowed: String = client.request("eth_shadowed", rpc_params![]).await.unwrap();
        assert_eq!(shadowed, "local");
    }

    #[tokio::test]
    async fn preserves_upstream_errors() {
        let (upstream_addr, _upstream) = upstream_server().await;
        let (front_addr, _front) = front_server(upstream_addr).await;
        let client = HttpClientBuilder::default().build(format!("http://{front_addr}")).unwrap();

        let err = client.request::<(), _>("eth_fail", rpc_params![]).await.unwrap_err();
        let ClientError::Call(call) = err else { panic!("expected call error, got {err:?}") };
        assert_eq!(call.code(), 3);
        assert_eq!(call.message(), "upstream failure");

        let err = client.request::<(), _>("eth_missing", rpc_params![]).await.unwrap_err();
        let ClientError::Call(call) = err else { panic!("expected call error, got {err:?}") };
        assert_eq!(call.code(), ErrorCode::MethodNotFound.code());
    }

    #[tokio::test]
    async fn routes_batch_entries_individually() {
        let (upstream_addr, _upstream) = upstream_server().await;
        let (front_addr, _front) = front_server(upstream_addr).await;
        let client = HttpClientBuilder::default().build(format!("http://{front_addr}")).unwrap();

        let mut batch = BatchRequestBuilder::new();
        batch.insert("optimism_syncStatus", rpc_params![]).unwrap();
        batch.insert("eth_chainId", rpc_params![]).unwrap();
        let responses = client.batch_request::<String>(batch).await.unwrap();
        let results: Vec<_> = responses.into_ok().unwrap().collect();
        assert_eq!(results, ["local", "0x2105"]);
    }
}
