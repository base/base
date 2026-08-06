//! Tower layer that injects W3C `traceparent` / `tracestate` headers into outbound HTTP requests.

use std::task::{Context as TaskContext, Poll};

use http::{Request, Response};
use opentelemetry::global;
use opentelemetry_http::HeaderInjector;
use tower::{Layer, Service};

/// Tower layer that injects W3C `traceparent` headers into outbound HTTP requests.
#[derive(Clone, Debug, Default)]
pub struct TraceContextLayer;

impl<S> Layer<S> for TraceContextLayer {
    type Service = TraceContextService<S>;

    fn layer(&self, inner: S) -> Self::Service {
        TraceContextService { inner }
    }
}

/// Tower service that injects W3C trace context into outbound HTTP requests.
#[derive(Clone, Debug)]
pub struct TraceContextService<S> {
    inner: S,
}

impl<S, ReqBody, ResBody> Service<Request<ReqBody>> for TraceContextService<S>
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
        // Keep this middleware in sync with execution/rpc trace_middleware extraction logic.
        global::get_text_map_propagator(|propagator| {
            propagator.inject_context(
                &opentelemetry::Context::current(),
                &mut HeaderInjector(req.headers_mut()),
            );
        });
        self.inner.call(req)
    }
}
