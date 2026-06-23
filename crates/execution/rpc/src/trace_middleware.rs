//! Middleware for extracting and applying W3C trace context from inbound JSON-RPC HTTP requests.
//!
//! Flow:
//!   HTTP layer: reads `traceparent`/`tracestate` headers → extracts [`opentelemetry::Context`] → inserts as
//!               `InboundOtelContext` extension on the HTTP request
//!   RPC layer:  reads `InboundOtelContext` from RPC request extensions → attaches as current
//!               context so that `#[instrument]` spans on RPC handlers become children of the
//!               caller's trace

use std::{
    future::Future,
    task::{Context as TaskContext, Poll},
};

use http::{Request, Response};
use jsonrpsee_core::middleware::{Batch, Notification, Request as RpcRequest, RpcServiceT};
use opentelemetry::{
    Context, global,
    trace::{FutureExt, TraceContextExt},
};
use opentelemetry_http::HeaderExtractor;
use tower::{Layer, Service};

/// Inbound [`opentelemetry::Context`] extracted from request headers.
#[derive(Clone, Debug)]
pub struct InboundOtelContext(pub Context);

/// Tower layer that extracts W3C trace context from inbound HTTP headers.
#[derive(Clone, Debug, Default)]
pub struct OtelHttpMiddlewareLayer;

impl<S> Layer<S> for OtelHttpMiddlewareLayer {
    type Service = OtelHttpMiddleware<S>;

    fn layer(&self, inner: S) -> Self::Service {
        OtelHttpMiddleware { inner }
    }
}

/// Tower service that stores extracted context in request extensions.
#[derive(Clone, Debug)]
pub struct OtelHttpMiddleware<S> {
    inner: S,
}

impl<S, ReqBody, ResBody> Service<Request<ReqBody>> for OtelHttpMiddleware<S>
where
    S: Service<Request<ReqBody>, Response = Response<ResBody>>,
{
    type Response = S::Response;
    type Error = S::Error;
    type Future = S::Future;

    fn poll_ready(&mut self, cx: &mut TaskContext<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, mut req: Request<ReqBody>) -> Self::Future {
        let cx = global::get_text_map_propagator(|propagator| {
            propagator.extract(&HeaderExtractor(req.headers()))
        });

        if cx.span().span_context().is_valid() {
            req.extensions_mut().insert(InboundOtelContext(cx));
        }

        self.inner.call(req)
    }
}

/// RPC middleware layer that attaches extracted inbound context.
#[derive(Clone, Debug, Default)]
pub struct OtelRpcMiddlewareLayer;

impl<S> Layer<S> for OtelRpcMiddlewareLayer {
    type Service = OtelRpcMiddleware<S>;

    fn layer(&self, inner: S) -> Self::Service {
        OtelRpcMiddleware { inner }
    }
}

/// RPC middleware that sets parent [`opentelemetry::Context`] for method handling.
#[derive(Clone, Debug)]
pub struct OtelRpcMiddleware<S> {
    inner: S,
}

impl<S> RpcServiceT for OtelRpcMiddleware<S>
where
    S: RpcServiceT + Send + Sync + Clone + 'static,
{
    type BatchResponse = S::BatchResponse;
    type MethodResponse = S::MethodResponse;
    type NotificationResponse = S::NotificationResponse;

    fn call<'a>(
        &self,
        req: RpcRequest<'a>,
    ) -> impl Future<Output = Self::MethodResponse> + Send + 'a {
        let cx = req.extensions().get::<InboundOtelContext>().cloned();
        let inner = self.inner.clone();

        async move {
            if let Some(InboundOtelContext(parent_ctx)) = cx {
                inner.call(req).with_context(parent_ctx).await
            } else {
                inner.call(req).await
            }
        }
    }

    fn batch<'a>(
        &self,
        mut req: Batch<'a>,
    ) -> impl Future<Output = Self::BatchResponse> + Send + 'a {
        let cx = req.extensions().get::<InboundOtelContext>().cloned();
        let inner = self.inner.clone();

        async move {
            if let Some(InboundOtelContext(parent_ctx)) = cx {
                inner.batch(req).with_context(parent_ctx).await
            } else {
                inner.batch(req).await
            }
        }
    }

    fn notification<'a>(
        &self,
        req: Notification<'a>,
    ) -> impl Future<Output = Self::NotificationResponse> + Send + 'a {
        let cx = req.extensions().get::<InboundOtelContext>().cloned();
        let inner = self.inner.clone();

        async move {
            if let Some(InboundOtelContext(parent_ctx)) = cx {
                inner.notification(req).with_context(parent_ctx).await
            } else {
                inner.notification(req).await
            }
        }
    }
}
