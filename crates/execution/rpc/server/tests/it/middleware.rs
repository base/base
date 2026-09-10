use std::{
    future::Future,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};

use base_execution_rpc_handlers::EthApiClient;
use base_execution_rpc_server::{RpcServerConfig, TransportRpcModuleConfig};
use jsonrpsee::{
    core::middleware::{Batch, Notification},
    server::middleware::rpc::RpcServiceT,
    types::Request,
};
use tower::Layer;

use crate::utils::{test_address, test_rpc_registry};

#[derive(Clone, Default)]
struct MyMiddlewareLayer {
    count: Arc<AtomicUsize>,
}

impl<S> Layer<S> for MyMiddlewareLayer {
    type Service = MyMiddlewareService<S>;

    fn layer(&self, inner: S) -> Self::Service {
        MyMiddlewareService { service: inner, count: self.count.clone() }
    }
}

#[derive(Clone)]
struct MyMiddlewareService<S> {
    service: S,
    count: Arc<AtomicUsize>,
}

impl<S> RpcServiceT for MyMiddlewareService<S>
where
    S: RpcServiceT + Send + Sync + Clone + 'static,
{
    type MethodResponse = S::MethodResponse;
    type NotificationResponse = S::NotificationResponse;
    type BatchResponse = S::BatchResponse;

    fn call<'a>(&self, req: Request<'a>) -> impl Future<Output = Self::MethodResponse> + Send + 'a {
        tracing::info!("MyMiddleware processed call {}", req.method);
        let count = self.count.clone();
        let service = self.service.clone();
        async move {
            let rp = service.call(req).await;
            // Modify the state.
            count.fetch_add(1, Ordering::Relaxed);
            rp
        }
    }

    fn batch<'a>(&self, req: Batch<'a>) -> impl Future<Output = Self::BatchResponse> + Send + 'a {
        self.service.batch(req)
    }

    fn notification<'a>(
        &self,
        n: Notification<'a>,
    ) -> impl Future<Output = Self::NotificationResponse> + Send + 'a {
        self.service.notification(n)
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn test_rpc_middleware() {
    let mut registry = test_rpc_registry().await;
    let modules = registry.create_transport_rpc_modules(TransportRpcModuleConfig::set_http());

    let mylayer = MyMiddlewareLayer::default();

    let handle = RpcServerConfig::http(Default::default())
        .with_http_address(test_address())
        .set_rpc_middleware(mylayer.clone())
        .start(&modules)
        .await
        .unwrap();

    let client = handle.http_client().unwrap();
    EthApiClient::protocol_version(&client).await.unwrap();
    let count = mylayer.count.load(Ordering::Relaxed);
    assert_eq!(count, 1);
}
