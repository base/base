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
    pin::Pin,
    task::{Context as TaskContext, Poll},
};

use http::{Request, Response};
use jsonrpsee_core::middleware::{Batch, Notification, Request as RpcRequest, RpcServiceT};
use opentelemetry::{Context, global, trace::TraceContextExt};
use opentelemetry_http::HeaderExtractor;
use pin_project_lite::pin_project;
use tower::{Layer, Service};
use tracing::Instrument;

pin_project! {
    /// Future wrapper that keeps an extracted OTel context attached while the request is polled.
    #[derive(Debug)]
    pub struct OtelHttpMiddlewareFuture<F> {
        #[pin]
        inner: F,
        otel_cx: Option<InboundOtelContext>,
    }
}

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
    type Future = OtelHttpMiddlewareFuture<S::Future>;

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

        let otel_cx = req.extensions().get::<InboundOtelContext>().cloned();

        OtelHttpMiddlewareFuture { inner: self.inner.call(req), otel_cx }
    }
}

impl<F> Future for OtelHttpMiddlewareFuture<F>
where
    F: Future,
{
    type Output = F::Output;

    fn poll(self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<Self::Output> {
        // Keep the inbound context current while the auth/engine request future is polled so
        // handler spans created inside that future inherit the caller's traceparent.
        let this = self.project();
        let _guard =
            this.otel_cx.as_ref().cloned().map(|InboundOtelContext(otel_cx)| otel_cx.attach());
        this.inner.poll(cx)
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
        // Attach the parent OTel context before creating the tracing span so that
        // tracing-opentelemetry sets it as the span's OTel parent at construction time.
        // The guard is dropped immediately after; the span retains the parent reference.
        let _guard = cx.map(|InboundOtelContext(ctx)| ctx.attach());
        let span =
            tracing::info_span!("request", "otel.kind" = "server", "rpc.method" = %req.method);
        drop(_guard);
        let inner = self.inner.clone();
        async move { inner.call(req).await }.instrument(span)
    }

    fn batch<'a>(
        &self,
        mut req: Batch<'a>,
    ) -> impl Future<Output = Self::BatchResponse> + Send + 'a {
        let cx = req.extensions().get::<InboundOtelContext>().cloned();
        let _guard = cx.map(|InboundOtelContext(ctx)| ctx.attach());
        let span =
            tracing::info_span!("batch", "otel.kind" = "server", "rpc.batch.len" = req.len());
        drop(_guard);
        let inner = self.inner.clone();
        async move { inner.batch(req).await }.instrument(span)
    }

    fn notification<'a>(
        &self,
        req: Notification<'a>,
    ) -> impl Future<Output = Self::NotificationResponse> + Send + 'a {
        let cx = req.extensions().get::<InboundOtelContext>().cloned();
        let _guard = cx.map(|InboundOtelContext(ctx)| ctx.attach());
        let span =
            tracing::info_span!("notification", "otel.kind" = "server", "rpc.method" = %req.method);
        drop(_guard);
        let inner = self.inner.clone();
        async move { inner.notification(req).await }.instrument(span)
    }
}
