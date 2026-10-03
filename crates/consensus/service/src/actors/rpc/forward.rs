//! RPC middleware that forwards methods the consensus server does not serve to an upstream server.
//!
//! Installing this middleware lets the consensus RPC endpoint answer both the consensus namespaces
//! it registers itself and every other method by proxying it to an execution node's HTTP server.
//! Batches are split by backend: locally served entries run in-process and the rest travel upstream
//! as a single batch, with responses returned in the original order under the original ids.

use std::{borrow::Cow, collections::HashSet, sync::Arc, time::Duration};

use futures::future::join_all;
use jsonrpsee::{
    RpcModule,
    core::{
        ClientError, JsonRawValue,
        client::ClientT,
        middleware::{Batch, BatchEntry, Notification, RpcServiceT},
        params::BatchRequestBuilder,
        server::{BatchResponseBuilder, MethodResponse, ResponsePayload},
        traits::ToRpcParams,
    },
    http_client::{HttpClient, HttpClientBuilder},
    types::{ErrorCode, ErrorObject, ErrorObjectOwned, Extensions, Id, Request},
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

/// A call within a batch that is bound for the upstream.
struct UpstreamCall {
    method: String,
    params: Option<Box<JsonRawValue>>,
}

/// The outcome of one forwarded call.
enum UpstreamResult {
    /// The upstream returned a result.
    Success(Box<JsonRawValue>),
    /// The upstream returned a JSON-RPC error, which is passed on unchanged.
    Rejected(ErrorObjectOwned),
    /// The upstream could not be reached or returned an unusable response.
    Unavailable,
}

impl Upstream {
    /// Returns whether `method` is served by this server rather than the upstream.
    fn serves_locally(&self, method: &str) -> bool {
        self.local_methods.contains(method)
    }

    /// Forwards `calls` as a single upstream batch and returns one result per call, in order.
    async fn call_batch(&self, calls: &[UpstreamCall]) -> Vec<UpstreamResult> {
        if calls.is_empty() {
            return Vec::new();
        }

        let mut batch = BatchRequestBuilder::new();
        for call in calls {
            if batch.insert(&call.method, RawParams(call.params.clone())).is_err() {
                return calls.iter().map(|_| UpstreamResult::Unavailable).collect();
            }
        }

        match self.client.batch_request::<Box<JsonRawValue>>(batch).await {
            Ok(responses) => responses
                .into_iter()
                .map(|entry| match entry {
                    Ok(result) => UpstreamResult::Success(result),
                    Err(err) => UpstreamResult::Rejected(err.into_owned()),
                })
                .collect(),
            Err(err) => {
                warn!(target: "rpc", calls = calls.len(), %err, "failed to forward batch upstream");
                calls.iter().map(|_| UpstreamResult::Unavailable).collect()
            }
        }
    }

    /// Forwards a notification, which has no response to return.
    async fn notify(&self, call: UpstreamCall) {
        if let Err(err) = self.client.notification(&call.method, RawParams(call.params)).await {
            warn!(target: "rpc", method = %call.method, %err, "failed to forward notification upstream");
        }
    }
}

impl UpstreamResult {
    /// Converts the outcome into the response for request `id`.
    fn into_response(self, id: Id<'static>) -> MethodResponse {
        match self {
            Self::Success(result) => {
                MethodResponse::response(id, ResponsePayload::success(result), MAX_RESPONSE_SIZE)
            }
            Self::Rejected(err) => {
                MethodResponse::response(id, ResponsePayload::<()>::error(err), MAX_RESPONSE_SIZE)
            }
            Self::Unavailable => {
                MethodResponse::error(id, ErrorObject::from(ErrorCode::InternalError))
            }
        }
    }
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
            if upstream.serves_locally(&req.method) {
                return service.call(req).await;
            }

            let Request { id, method, params, extensions, .. } = req;
            let params = RawParams(params.map(Cow::into_owned));
            let result =
                match upstream.client.request::<Box<JsonRawValue>, _>(&method, params).await {
                    Ok(result) => UpstreamResult::Success(result),
                    Err(ClientError::Call(err)) => UpstreamResult::Rejected(err),
                    Err(err) => {
                        warn!(target: "rpc", %method, %err, "failed to forward request upstream");
                        UpstreamResult::Unavailable
                    }
                };
            result.into_response(id.into_owned()).with_extensions(extensions)
        }
    }
}

/// Where the response for one batch entry comes from.
enum BatchSlot {
    /// Already answered, either locally or by a malformed entry.
    Ready(MethodResponse),
    /// Answered by the upstream batch result at `index`.
    Upstream { index: usize, id: Id<'static>, extensions: Extensions },
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
        // The inner service's `batch` dispatches every entry to its own `call`, which would bypass
        // this middleware, so entries are routed here instead. Locally served entries run
        // in-process and the rest go upstream together as one batch.
        let service = self.service.clone();
        let upstream = Arc::clone(&self.upstream);
        async move {
            let mut slots = Vec::with_capacity(batch.len());
            let mut calls = Vec::new();
            let mut notifications = Vec::new();
            let mut got_notification = false;

            for entry in batch {
                match entry {
                    Ok(BatchEntry::Call(req)) if upstream.serves_locally(&req.method) => {
                        slots.push(BatchSlot::Ready(service.call(req).await));
                    }
                    Ok(BatchEntry::Call(req)) => {
                        let Request { id, method, params, extensions, .. } = req;
                        slots.push(BatchSlot::Upstream {
                            index: calls.len(),
                            id: id.into_owned(),
                            extensions,
                        });
                        calls.push(UpstreamCall {
                            method: method.into_owned(),
                            params: params.map(Cow::into_owned),
                        });
                    }
                    Ok(BatchEntry::Notification(n)) => {
                        got_notification = true;
                        if upstream.serves_locally(&n.method) {
                            service.notification(n).await;
                        } else {
                            notifications.push(UpstreamCall {
                                method: n.method.into_owned(),
                                params: n.params.map(Cow::into_owned),
                            });
                        }
                    }
                    Err(err) => {
                        let (err, id) = err.into_parts();
                        slots.push(BatchSlot::Ready(MethodResponse::error(id, err)));
                    }
                }
            }

            let (results, _) = futures::join!(
                upstream.call_batch(&calls),
                join_all(notifications.into_iter().map(|call| upstream.notify(call)))
            );
            let mut results: Vec<_> = results.into_iter().map(Some).collect();

            let mut responses = BatchResponseBuilder::new_with_limit(MAX_RESPONSE_SIZE);
            for slot in slots {
                let response = match slot {
                    BatchSlot::Ready(response) => response,
                    BatchSlot::Upstream { index, id, extensions } => results[index]
                        .take()
                        .unwrap_or(UpstreamResult::Unavailable)
                        .into_response(id)
                        .with_extensions(extensions),
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
        let service = self.service.clone();
        let upstream = Arc::clone(&self.upstream);
        async move {
            if upstream.serves_locally(&n.method) {
                return service.notification(n).await;
            }
            let extensions = n.extensions.clone();
            upstream
                .notify(UpstreamCall {
                    method: n.method.into_owned(),
                    params: n.params.map(Cow::into_owned),
                })
                .await;
            MethodResponse::notification().with_extensions(extensions)
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{
        net::SocketAddr,
        num::NonZeroUsize,
        sync::atomic::{AtomicUsize, Ordering},
    };

    use jsonrpsee::{
        core::{client::ClientT, params::BatchRequestBuilder},
        http_client::HttpClientBuilder,
        rpc_params,
        server::{Server, ServerHandle, middleware::rpc::RpcServiceBuilder},
    };

    use super::*;
    use crate::actors::rpc::actor::bind_rpc_server;

    /// Upstream middleware that counts the batch requests it receives.
    #[derive(Clone)]
    struct CountBatches<S> {
        service: S,
        batches: Arc<AtomicUsize>,
    }

    impl<S> RpcServiceT for CountBatches<S>
    where
        S: RpcServiceT + Clone + Send + Sync + 'static,
    {
        type MethodResponse = S::MethodResponse;
        type NotificationResponse = S::NotificationResponse;
        type BatchResponse = S::BatchResponse;

        fn call<'a>(
            &self,
            req: Request<'a>,
        ) -> impl Future<Output = Self::MethodResponse> + Send + 'a {
            self.service.call(req)
        }

        fn batch<'a>(
            &self,
            batch: Batch<'a>,
        ) -> impl Future<Output = Self::BatchResponse> + Send + 'a {
            self.batches.fetch_add(1, Ordering::SeqCst);
            self.service.batch(batch)
        }

        fn notification<'a>(
            &self,
            n: Notification<'a>,
        ) -> impl Future<Output = Self::NotificationResponse> + Send + 'a {
            self.service.notification(n)
        }
    }

    struct UpstreamServer {
        addr: SocketAddr,
        batches: Arc<AtomicUsize>,
        _handle: ServerHandle,
    }

    async fn upstream_server() -> UpstreamServer {
        let batches = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&batches);
        let server =
            Server::builder()
                .set_rpc_middleware(RpcServiceBuilder::new().layer_fn(move |service| {
                    CountBatches { service, batches: Arc::clone(&counter) }
                }))
                .build("127.0.0.1:0")
                .await
                .unwrap();
        let mut module = RpcModule::new(());
        module.register_method("eth_chainId", |_, _, _| "0x2105").unwrap();
        module.register_method("eth_blockNumber", |_, _, _| "0x10").unwrap();
        module.register_method("eth_shadowed", |_, _, _| "upstream").unwrap();
        module
            .register_async_method("eth_slow", |_, _, _| async {
                tokio::time::sleep(Duration::from_secs(30)).await;
                "late"
            })
            .unwrap();
        module
            .register_method("eth_fail", |_, _, _| {
                Err::<(), _>(ErrorObjectOwned::owned(3, "upstream failure", None::<()>))
            })
            .unwrap();
        let addr = server.local_addr().unwrap();
        UpstreamServer { addr, batches, _handle: server.start(module) }
    }

    async fn front_server(upstream: SocketAddr) -> (SocketAddr, ServerHandle) {
        front_server_with_timeout(upstream, Duration::from_secs(5)).await
    }

    async fn front_server_with_timeout(
        upstream: SocketAddr,
        http_timeout: Duration,
    ) -> (SocketAddr, ServerHandle) {
        let mut module = RpcModule::new(());
        module.register_method("optimism_syncStatus", |_, _, _| "local").unwrap();
        module.register_method("admin_status", |_, _, _| "admin").unwrap();
        module.register_method("eth_shadowed", |_, _, _| "local").unwrap();
        let config = base_consensus_rpc::RpcBuilder {
            socket: "127.0.0.1:0".parse().unwrap(),
            no_restart: true,
            enable_admin: false,
            admin_persistence: None,
            ws_enabled: false,
            dev_enabled: false,
            http_timeout,
            max_concurrent_requests: NonZeroUsize::new(16).expect("nonzero"),
            forward_unmatched_to: Some(Url::parse(&format!("http://{upstream}")).unwrap()),
        };
        bind_rpc_server(&config, module).await.unwrap()
    }

    async fn post(front: SocketAddr, body: &str) -> String {
        reqwest::Client::new()
            .post(format!("http://{front}"))
            .header("content-type", "application/json")
            .body(body.to_owned())
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn forwards_unmatched_methods_and_keeps_local_ones() {
        let upstream = upstream_server().await;
        let (front_addr, _front) = front_server(upstream.addr).await;
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
        let upstream = upstream_server().await;
        let (front_addr, _front) = front_server(upstream.addr).await;
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
    async fn mixed_batch_returns_each_entry_from_its_backend_in_order() {
        let upstream = upstream_server().await;
        let (front_addr, _front) = front_server(upstream.addr).await;
        let client = HttpClientBuilder::default().build(format!("http://{front_addr}")).unwrap();

        let mut batch = BatchRequestBuilder::new();
        for method in [
            "eth_chainId",
            "optimism_syncStatus",
            "eth_fail",
            "eth_shadowed",
            "eth_missing",
            "admin_status",
            "eth_blockNumber",
        ] {
            batch.insert(method, rpc_params![]).unwrap();
        }
        let responses = client.batch_request::<String>(batch).await.unwrap();

        let results: Vec<_> = responses
            .into_iter()
            .map(|entry| entry.map_err(|err| (err.code(), err.message().to_owned())))
            .collect();
        assert_eq!(
            results,
            [
                Ok("0x2105".to_owned()),
                Ok("local".to_owned()),
                Err((3, "upstream failure".to_owned())),
                Ok("local".to_owned()),
                Err((ErrorCode::MethodNotFound.code(), "Method not found".to_owned())),
                Ok("admin".to_owned()),
                Ok("0x10".to_owned()),
            ]
        );
    }

    #[tokio::test]
    async fn sends_forwarded_batch_entries_upstream_as_one_batch() {
        let upstream = upstream_server().await;
        let (front_addr, _front) = front_server(upstream.addr).await;
        let client = HttpClientBuilder::default().build(format!("http://{front_addr}")).unwrap();

        let mut batch = BatchRequestBuilder::new();
        for method in ["eth_chainId", "optimism_syncStatus", "eth_blockNumber", "eth_chainId"] {
            batch.insert(method, rpc_params![]).unwrap();
        }
        client.batch_request::<String>(batch).await.unwrap().into_ok().unwrap().for_each(drop);
        assert_eq!(upstream.batches.load(Ordering::SeqCst), 1);

        // A batch served entirely in-process never reaches the upstream.
        let mut batch = BatchRequestBuilder::new();
        batch.insert("optimism_syncStatus", rpc_params![]).unwrap();
        batch.insert("admin_status", rpc_params![]).unwrap();
        client.batch_request::<String>(batch).await.unwrap().into_ok().unwrap().for_each(drop);
        assert_eq!(upstream.batches.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn batch_keeps_caller_request_ids() {
        let upstream = upstream_server().await;
        let (front_addr, _front) = front_server(upstream.addr).await;

        let body = post(
            front_addr,
            r#"[{"jsonrpc":"2.0","id":"a","method":"eth_chainId"},
                {"jsonrpc":"2.0","id":7,"method":"optimism_syncStatus"},
                {"jsonrpc":"2.0","id":"c","method":"eth_blockNumber"}]"#,
        )
        .await;

        assert_eq!(
            body,
            r#"[{"jsonrpc":"2.0","id":"a","result":"0x2105"},{"jsonrpc":"2.0","id":7,"result":"local"},{"jsonrpc":"2.0","id":"c","result":"0x10"}]"#
        );
    }

    #[tokio::test]
    async fn batch_notifications_get_no_response() {
        let upstream = upstream_server().await;
        let (front_addr, _front) = front_server(upstream.addr).await;

        let mixed = post(
            front_addr,
            r#"[{"jsonrpc":"2.0","method":"eth_chainId"},
                {"jsonrpc":"2.0","method":"optimism_syncStatus"},
                {"jsonrpc":"2.0","id":1,"method":"eth_blockNumber"}]"#,
        )
        .await;
        assert_eq!(mixed, r#"[{"jsonrpc":"2.0","id":1,"result":"0x10"}]"#);

        // A batch of only notifications answers exactly as a server without forwarding does.
        let body = r#"[{"jsonrpc":"2.0","method":"eth_chainId"}]"#;
        assert_eq!(post(front_addr, body).await, post(upstream.addr, body).await);
    }

    #[tokio::test]
    async fn unreachable_upstream_fails_only_forwarded_batch_entries() {
        // Nothing listens on the upstream address once its server is dropped.
        let unused = {
            let upstream = upstream_server().await;
            upstream.addr
        };
        let (front_addr, _front) = front_server(unused).await;
        let client = HttpClientBuilder::default().build(format!("http://{front_addr}")).unwrap();

        let mut batch = BatchRequestBuilder::new();
        for method in ["eth_chainId", "optimism_syncStatus", "eth_blockNumber"] {
            batch.insert(method, rpc_params![]).unwrap();
        }
        let results: Vec<_> = client
            .batch_request::<String>(batch)
            .await
            .unwrap()
            .into_iter()
            .map(|entry| entry.map_err(|err| err.code()))
            .collect();

        assert_eq!(
            results,
            [
                Err(ErrorCode::InternalError.code()),
                Ok("local".to_owned()),
                Err(ErrorCode::InternalError.code()),
            ]
        );
    }

    #[tokio::test]
    async fn slow_upstream_times_out_as_a_json_rpc_error_not_an_http_timeout() {
        let upstream = upstream_server().await;
        let (front_addr, _front) =
            front_server_with_timeout(upstream.addr, Duration::from_secs(1)).await;

        let response = reqwest::Client::new()
            .post(format!("http://{front_addr}"))
            .header("content-type", "application/json")
            .body(r#"{"jsonrpc":"2.0","id":1,"method":"eth_slow"}"#)
            .send()
            .await
            .unwrap();

        assert_eq!(response.status(), reqwest::StatusCode::OK);
        let body: serde_json::Value = response.json().await.unwrap();
        assert_eq!(body["error"]["code"], ErrorCode::InternalError.code());
    }
}
