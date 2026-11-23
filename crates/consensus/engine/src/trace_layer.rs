//! Tower layer that injects W3C `traceparent` / `tracestate` headers into outbound JSON-RPC
//! [`RequestPacket`]s.

use std::task::{Context as TaskContext, Poll};

use alloy_json_rpc::{RequestPacket, ResponsePacket};
use alloy_transport::TransportError;
use opentelemetry::global;
use opentelemetry_http::HeaderInjector;
use tower::{Layer, Service};

/// Tower layer that injects W3C `traceparent` headers into outbound JSON-RPC [`RequestPacket`]s.
#[derive(Clone, Debug, Default)]
pub struct TraceContextLayer;

impl<S> Layer<S> for TraceContextLayer {
    type Service = TraceContextService<S>;

    fn layer(&self, inner: S) -> Self::Service {
        TraceContextService { inner }
    }
}

/// Tower service that injects W3C trace context into outbound JSON-RPC [`RequestPacket`]s.
#[derive(Clone, Debug)]
pub struct TraceContextService<S> {
    inner: S,
}

impl<S> Service<RequestPacket> for TraceContextService<S>
where
    S: Service<RequestPacket, Response = ResponsePacket, Error = TransportError>,
{
    type Response = S::Response;
    type Error = S::Error;
    type Future = S::Future;

    fn poll_ready(&mut self, cx: &mut TaskContext<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, mut req: RequestPacket) -> Self::Future {
        let headers = match &mut req {
            RequestPacket::Single(single) => Some(single.headers_mut()),
            RequestPacket::Batch(batch) => batch.first_mut().map(|first| first.headers_mut()),
        };
        if let Some(headers) = headers {
            // Keep this middleware in sync with execution/rpc trace_middleware extraction logic.
            global::get_text_map_propagator(|propagator| {
                propagator.inject_context(
                    &opentelemetry::Context::current(),
                    &mut HeaderInjector(headers),
                );
            });
        }
        self.inner.call(req)
    }
}
