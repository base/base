//! RPC middleware that forwards the methods the consensus RPC server does not serve to the
//! execution client.

use std::borrow::Cow;

use futures::future::Either;
use jsonrpsee::{
    Methods,
    core::{
        ClientError, JsonRawValue, TEN_MB_SIZE_BYTES,
        client::ClientT,
        middleware::{Batch, BatchEntry, Notification, Request, RpcServiceT},
        server::{BatchResponseBuilder, MethodResponse, ResponsePayload},
        traits::ToRpcParams,
    },
    http_client::HttpClient,
    types::{ErrorObject, error::INTERNAL_ERROR_CODE},
};
use tower::Layer;

/// jsonrpsee's default response size limit, which the server and the forwarding client both
/// run with.
const MAX_RESPONSE_SIZE: usize = TEN_MB_SIZE_BYTES as usize;

/// RPC middleware layer that wraps a service in [`ExecutionForwarding`].
#[derive(Clone, Debug)]
pub struct ExecutionForwardingLayer {
    client: HttpClient,
    served_methods: Methods,
}

impl ExecutionForwardingLayer {
    /// Creates a layer that forwards to `client` every method `served_methods` does not
    /// register.
    pub const fn new(client: HttpClient, served_methods: Methods) -> Self {
        Self { client, served_methods }
    }
}

impl<S> Layer<S> for ExecutionForwardingLayer {
    type Service = ExecutionForwarding<S>;

    fn layer(&self, inner: S) -> Self::Service {
        ExecutionForwarding {
            inner,
            client: self.client.clone(),
            served_methods: self.served_methods.clone(),
        }
    }
}

/// RPC middleware that passes the methods the server serves to the inner service and forwards
/// every other method to the execution client's HTTP RPC, params untouched, returning its
/// result or error as is.
///
/// The exact method name decides, so a served method is never forwarded. Each entry of a batch
/// is served or forwarded on its own, and notifications are not forwarded. A forwarded call
/// fails with an internal error when no valid JSON-RPC response comes back, including a
/// response over the forwarding client's 10 MB limit.
#[derive(Clone, Debug)]
pub struct ExecutionForwarding<S> {
    inner: S,
    client: HttpClient,
    served_methods: Methods,
}

impl<S> ExecutionForwarding<S> {
    /// Forwards `req` to the execution client and returns its result or error as is, or an
    /// internal error when no valid JSON-RPC response comes back.
    async fn forward(client: HttpClient, req: Request<'_>) -> MethodResponse {
        let result: Result<Box<JsonRawValue>, _> =
            client.request(&req.method, ForwardedParams(req.params)).await;

        let payload = match result {
            Ok(result) => ResponsePayload::success(result),
            Err(ClientError::Call(error)) => ResponsePayload::error(error),
            Err(error) => {
                // Only the logs get the cause, which can reveal how the execution client is
                // reached.
                warn!(target: "rpc", %error, "failed to forward to the execution client");
                ResponsePayload::error(ErrorObject::owned(
                    INTERNAL_ERROR_CODE,
                    "forwarding to the execution client failed",
                    None::<()>,
                ))
            }
        };
        MethodResponse::response(req.id, payload, MAX_RESPONSE_SIZE).with_extensions(req.extensions)
    }
}

impl<S> RpcServiceT for ExecutionForwarding<S>
where
    S: RpcServiceT<MethodResponse = MethodResponse, NotificationResponse = MethodResponse>
        + Send
        + Sync
        + Clone
        + 'static,
{
    type BatchResponse = MethodResponse;
    type MethodResponse = MethodResponse;
    type NotificationResponse = MethodResponse;

    fn call<'a>(&self, req: Request<'a>) -> impl Future<Output = MethodResponse> + Send + 'a {
        if self.served_methods.method(req.method_name()).is_some() {
            return Either::Left(self.inner.call(req));
        }
        Either::Right(Self::forward(self.client.clone(), req))
    }

    fn batch<'a>(&self, batch: Batch<'a>) -> impl Future<Output = MethodResponse> + Send + 'a {
        // The inner service dispatches the entries of a batch to its own `call`, which would
        // bypass the forwarding, so the entries are dispatched here.
        let this = self.clone();
        async move {
            let mut responses = BatchResponseBuilder::new_with_limit(MAX_RESPONSE_SIZE);
            let mut got_notification = false;
            for entry in batch {
                let response = match entry {
                    Ok(BatchEntry::Call(req)) => this.call(req).await,
                    Ok(BatchEntry::Notification(notification)) => {
                        got_notification = true;
                        this.notification(notification).await;
                        continue;
                    }
                    Err(invalid) => {
                        let (error, id) = invalid.into_parts();
                        MethodResponse::error(id, error)
                    }
                };
                if let Err(too_large) = responses.append(response) {
                    return too_large;
                }
            }

            // A batch made only of notifications is acknowledged like a single notification.
            if responses.is_empty() && got_notification {
                MethodResponse::notification()
            } else {
                MethodResponse::from_batch(responses.finish())
            }
        }
    }

    fn notification<'a>(
        &self,
        notification: Notification<'a>,
    ) -> impl Future<Output = MethodResponse> + Send + 'a {
        self.inner.notification(notification)
    }
}

/// The params of a forwarded request, passed on to the execution client without being parsed.
#[derive(Debug)]
pub struct ForwardedParams<'a>(pub Option<Cow<'a, JsonRawValue>>);

impl ToRpcParams for ForwardedParams<'_> {
    fn to_rpc_params(self) -> Result<Option<Box<JsonRawValue>>, serde_json::Error> {
        Ok(self.0.map(Cow::into_owned))
    }
}

#[cfg(test)]
mod tests {
    use std::{
        net::{Ipv4Addr, SocketAddr, TcpListener},
        num::NonZeroUsize,
        time::Duration,
    };

    use base_consensus_rpc::RpcBuilder;
    use jsonrpsee::{
        RpcModule,
        core::rpc_params,
        http_client::HttpClientBuilder,
        server::{Server, ServerHandle},
        types::{ErrorCode, ErrorObjectOwned, error::CALL_EXECUTION_FAILED_CODE},
    };
    use serde_json::{Value, json};

    use super::*;
    use crate::actors::launch_rpc_server;

    /// A method both the consensus server and [`FakeExecutionClient`] register.
    const METHOD_SERVED_BY_BOTH: &str = "admin_refreshUpgradeSignal";

    /// A stand-in for the execution client's HTTP RPC.
    struct FakeExecutionClient {
        addr: SocketAddr,
        handle: ServerHandle,
    }

    impl FakeExecutionClient {
        /// Starts on a free local port and serves `eth_echo`, which returns its raw params as a
        /// string, `eth_fail`, which fails with [`Self::error`], and [`METHOD_SERVED_BY_BOTH`],
        /// which returns `"execution"`.
        async fn start() -> Self {
            let mut module = RpcModule::new(());
            module
                .register_method("eth_echo", |params, _, _| params.as_str().map(str::to_owned))
                .unwrap();
            module.register_method("eth_fail", |_, _, _| Err::<(), _>(Self::error())).unwrap();
            module.register_method(METHOD_SERVED_BY_BOTH, |_, _, _| "execution").unwrap();

            let server = Server::builder().build((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
            let addr = server.local_addr().unwrap();
            Self { addr, handle: server.start(module) }
        }

        /// The error `eth_fail` answers with.
        fn error() -> ErrorObjectOwned {
            ErrorObject::owned(CALL_EXECUTION_FAILED_CODE, "execution reverted", Some("0xdead"))
        }
    }

    /// Launches the consensus RPC server with [`METHOD_SERVED_BY_BOTH`] as its only method,
    /// forwarding to `execution`, and returns its address and handle.
    async fn consensus_server(execution: SocketAddr) -> (SocketAddr, ServerHandle) {
        // Take a free port from the OS, since the launcher does not report the one it binds.
        let socket = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap().local_addr().unwrap();
        let config = RpcBuilder {
            socket,
            no_restart: true,
            enable_admin: false,
            admin_persistence: None,
            ws_enabled: false,
            dev_enabled: false,
            http_timeout: Duration::from_secs(5),
            max_concurrent_requests: NonZeroUsize::new(16).unwrap(),
            execution_forwarding_endpoint: Some(format!("http://{execution}").parse().unwrap()),
        };

        let mut module = RpcModule::new(());
        module.register_method(METHOD_SERVED_BY_BOTH, |_, _, _| "consensus").unwrap();
        (socket, launch_rpc_server(&config, module).await.unwrap())
    }

    /// A JSON-RPC client of the server at `addr`.
    fn client(addr: SocketAddr) -> HttpClient {
        HttpClientBuilder::default().build(format!("http://{addr}")).unwrap()
    }

    /// Posts the raw JSON-RPC `batch` to the server at `addr` and returns the response body.
    /// jsonrpsee's client cannot put a notification or an invalid entry in a batch.
    async fn post_batch(addr: SocketAddr, batch: Value) -> String {
        reqwest::Client::new()
            .post(format!("http://{addr}"))
            .header(http::header::CONTENT_TYPE, "application/json")
            .body(batch.to_string())
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap()
    }

    /// A method the server registers is served by it, even when the execution client has one
    /// of the same name.
    #[tokio::test]
    async fn a_served_method_is_not_forwarded() {
        let execution = FakeExecutionClient::start().await;
        let (addr, _handle) = consensus_server(execution.addr).await;

        let result: String =
            client(addr).request(METHOD_SERVED_BY_BOTH, rpc_params![]).await.unwrap();

        assert_eq!(result, "consensus");
    }

    /// A method the server does not register reaches the execution client with its params
    /// untouched, byte for byte, and its result comes back as is.
    #[tokio::test]
    async fn an_unserved_method_is_forwarded_with_its_params() {
        let execution = FakeExecutionClient::start().await;
        let (addr, _handle) = consensus_server(execution.addr).await;
        let params = rpc_params![7, json!({"full": [true, null]})];

        let result: String = client(addr).request("eth_echo", params).await.unwrap();

        assert_eq!(result, r#"[7,{"full":[true,null]}]"#);
    }

    /// An error of the execution client comes back with its code, message and data.
    #[tokio::test]
    async fn an_execution_client_error_is_returned_as_is() {
        let execution = FakeExecutionClient::start().await;
        let (addr, _handle) = consensus_server(execution.addr).await;

        let error = client(addr).request::<Value, _>("eth_fail", rpc_params![]).await.unwrap_err();

        assert!(
            matches!(&error, ClientError::Call(error) if *error == FakeExecutionClient::error()),
            "{error:?}"
        );
    }

    /// Once the execution client is down, a forwarded call fails with an internal error.
    #[tokio::test]
    async fn forwarding_to_a_stopped_execution_client_fails_with_an_internal_error() {
        let execution = FakeExecutionClient::start().await;
        let (addr, _handle) = consensus_server(execution.addr).await;
        execution.handle.stop().unwrap();
        execution.handle.stopped().await;

        let error = client(addr).request::<Value, _>("eth_echo", rpc_params![]).await.unwrap_err();

        assert!(
            matches!(&error, ClientError::Call(error) if error.code() == INTERNAL_ERROR_CODE),
            "{error:?}"
        );
    }

    /// Each entry of a batch is served or forwarded on its own and answered under its id. A
    /// notification gets no response and an invalid entry gets an invalid request error.
    #[tokio::test]
    async fn each_entry_of_a_batch_is_served_or_forwarded() {
        let execution = FakeExecutionClient::start().await;
        let (addr, _handle) = consensus_server(execution.addr).await;
        let batch = json!([
            {"jsonrpc": "2.0", "id": 1, "method": METHOD_SERVED_BY_BOTH},
            {"jsonrpc": "2.0", "id": "two", "method": "eth_echo", "params": ["latest"]},
            {"jsonrpc": "2.0", "method": "eth_echo"},
            {"jsonrpc": "2.0", "id": 3},
        ]);

        let responses: Value = serde_json::from_str(&post_batch(addr, batch).await).unwrap();

        assert_eq!(
            responses,
            json!([
                {"jsonrpc": "2.0", "id": 1, "result": "consensus"},
                {"jsonrpc": "2.0", "id": "two", "result": "[\"latest\"]"},
                {"jsonrpc": "2.0", "id": 3, "error": ErrorObject::from(ErrorCode::InvalidRequest)},
            ])
        );
    }

    /// A batch made only of notifications is acknowledged with `null`, as a single notification
    /// is, instead of being refused as an empty batch.
    #[tokio::test]
    async fn a_batch_of_notifications_is_acknowledged() {
        let execution = FakeExecutionClient::start().await;
        let (addr, _handle) = consensus_server(execution.addr).await;
        let batch = json!([{"jsonrpc": "2.0", "method": "eth_echo"}]);

        assert_eq!(post_batch(addr, batch).await, "null");
    }
}
