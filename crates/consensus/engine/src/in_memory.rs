//! In-process transport for an execution node embedded in the same binary.

use std::task::{Context, Poll};

use alloy_json_rpc::{RequestPacket, Response, ResponsePacket, SerializedRequest};
use alloy_transport::{TransportError, TransportErrorKind, TransportFut};
use jsonrpsee::RpcModule;
use tower::Service;

/// An alloy transport that dispatches requests straight into an in-process [`RpcModule`].
///
/// No socket, HTTP, or JWT is involved. Requests and responses are still JSON-encoded because
/// the module's method handlers only accept JSON-RPC.
///
/// This path does not apply the auth server's body limits. The auth HTTP and IPC servers cap
/// requests at 128 mebibytes and responses at 750 mebibytes; `raw_json_request` does not.
///
/// Each request runs on its own task, as it would on the auth server. A handler panic comes back
/// as a transport error, and dropping the caller's future does not cancel the handler.
#[derive(Clone, Debug)]
pub struct InMemoryTransport {
    /// The module that serves requests, typically the execution node's auth module.
    pub module: RpcModule<()>,
}

impl InMemoryTransport {
    /// Dispatches a single request to the module and decodes its response.
    pub async fn dispatch(&self, request: SerializedRequest) -> Result<Response, TransportError> {
        let module = self.module.clone();
        let request = request.into_serialized();
        let (response, _) =
            tokio::spawn(async move { module.raw_json_request(request.get(), 1).await })
                .await
                .map_err(TransportErrorKind::custom)?
                .map_err(TransportErrorKind::custom)?;
        serde_json::from_str(response.get()).map_err(TransportErrorKind::custom)
    }
}

impl Service<RequestPacket> for InMemoryTransport {
    type Response = ResponsePacket;
    type Error = TransportError;
    type Future = TransportFut<'static>;

    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, packet: RequestPacket) -> Self::Future {
        let this = self.clone();
        Box::pin(async move {
            match packet {
                RequestPacket::Single(request) => {
                    this.dispatch(request).await.map(ResponsePacket::Single)
                }
                RequestPacket::Batch(requests) => {
                    // ponytail: batch entries run sequentially; switch to join_all if batched
                    // engine calls ever become latency-sensitive.
                    let mut responses = Vec::with_capacity(requests.len());
                    for request in requests {
                        responses.push(this.dispatch(request).await?);
                    }
                    Ok(ResponsePacket::Batch(responses))
                }
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use alloy_rpc_client::RpcClient;
    use jsonrpsee::core::RpcResult;

    use super::*;

    fn panicking_handler() -> RpcResult<()> {
        panic!("handler panicked")
    }

    #[tokio::test]
    async fn handler_panic_is_a_transport_error() {
        let mut module = RpcModule::new(());
        module
            .register_async_method("test_panic", |_, _, _| async { panicking_handler() })
            .unwrap();
        let client = RpcClient::new(InMemoryTransport { module }, true);

        let result: Result<(), _> = client.request_noparams("test_panic").await;
        assert!(result.is_err());
    }
}
