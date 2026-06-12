use alloc::{string::String, vec::Vec};

use crate::ExecutionData;

/// W3C trace context headers carried alongside internal payload types.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TraceContextHeaders {
    /// W3C `traceparent` header value.
    pub traceparent: Option<String>,
    /// W3C `tracestate` header value.
    pub tracestate: Option<String>,
}

impl TraceContextHeaders {
    /// Creates a new trace context header container.
    pub const fn new(traceparent: Option<String>, tracestate: Option<String>) -> Self {
        Self { traceparent, tracestate }
    }

    /// Returns true if no trace headers are present.
    pub fn is_empty(&self) -> bool {
        self.traceparent.is_none() && self.tracestate.is_none()
    }

    /// Captures the current OTel context into W3C headers.
    #[cfg(feature = "reth")]
    pub fn from_current() -> Self {
        use opentelemetry::{Context, global, propagation::Injector};

        struct TraceContextInjector<'a>(&'a mut TraceContextHeaders);

        impl Injector for TraceContextInjector<'_> {
            fn set(&mut self, key: &str, value: String) {
                match key {
                    "traceparent" => self.0.traceparent = Some(value),
                    "tracestate" => self.0.tracestate = Some(value),
                    _ => {}
                }
            }
        }

        let mut headers = Self::default();
        let cx = Context::current();
        global::get_text_map_propagator(|propagator| {
            propagator.inject_context(&cx, &mut TraceContextInjector(&mut headers));
        });
        headers
    }

    /// Attaches these W3C headers as the current OTel parent context.
    #[cfg(feature = "reth")]
    pub fn attach_as_parent(&self) -> Option<opentelemetry::ContextGuard> {
        use opentelemetry::{global, propagation::Extractor, trace::TraceContextExt};

        struct TraceContextExtractor<'a>(&'a TraceContextHeaders);

        impl Extractor for TraceContextExtractor<'_> {
            fn get(&self, key: &str) -> Option<&str> {
                match key {
                    "traceparent" => self.0.traceparent.as_deref(),
                    "tracestate" => self.0.tracestate.as_deref(),
                    _ => None,
                }
            }

            fn keys(&self) -> Vec<&str> {
                let mut keys = Vec::with_capacity(2);
                if self.0.traceparent.is_some() {
                    keys.push("traceparent");
                }
                if self.0.tracestate.is_some() {
                    keys.push("tracestate");
                }
                keys
            }
        }

        if self.is_empty() {
            return None;
        }

        let cx = global::get_text_map_propagator(|propagator| {
            propagator.extract(&TraceContextExtractor(self))
        });
        cx.span().span_context().is_valid().then(|| cx.attach())
    }
}

/// Common interface for Base execution payload types that may carry trace context.
pub trait BaseExecutionDataExt: Clone {
    /// Returns the inner Base execution data.
    fn execution_data(&self) -> &ExecutionData;

    /// Returns the inner Base execution data by value.
    fn into_execution_data(self) -> ExecutionData;

    /// Returns any attached trace context headers.
    fn trace_context_headers(&self) -> Option<&TraceContextHeaders>;

    /// Returns a copy of this payload with the supplied trace context attached.
    fn with_trace_context(self, trace_context: TraceContextHeaders) -> Self;

    /// Captures the current OTel context and attaches it to this payload if supported.
    #[cfg(feature = "reth")]
    fn with_current_trace_context(self) -> Self {
        self.with_trace_context(TraceContextHeaders::from_current())
    }
}

/// Execution data wrapper that preserves inbound trace context across async/task boundaries.
#[derive(Clone, Debug)]
pub struct TracedExecutionData {
    /// The canonical Base execution payload.
    pub inner: ExecutionData,
    /// W3C trace context captured at ingress.
    pub trace_context: TraceContextHeaders,
}

impl TracedExecutionData {
    /// Creates a new traced execution payload with no trace context attached.
    pub const fn new(inner: ExecutionData) -> Self {
        Self { inner, trace_context: TraceContextHeaders::new(None, None) }
    }
}

impl BaseExecutionDataExt for ExecutionData {
    fn execution_data(&self) -> &ExecutionData {
        self
    }

    fn into_execution_data(self) -> ExecutionData {
        self
    }

    fn trace_context_headers(&self) -> Option<&TraceContextHeaders> {
        None
    }

    fn with_trace_context(self, _trace_context: TraceContextHeaders) -> Self {
        self
    }
}

impl BaseExecutionDataExt for TracedExecutionData {
    fn execution_data(&self) -> &ExecutionData {
        &self.inner
    }

    fn into_execution_data(self) -> ExecutionData {
        self.inner
    }

    fn trace_context_headers(&self) -> Option<&TraceContextHeaders> {
        (!self.trace_context.is_empty()).then_some(&self.trace_context)
    }

    fn with_trace_context(mut self, trace_context: TraceContextHeaders) -> Self {
        self.trace_context = trace_context;
        self
    }
}

impl From<ExecutionData> for TracedExecutionData {
    fn from(inner: ExecutionData) -> Self {
        Self::new(inner)
    }
}

impl From<TracedExecutionData> for ExecutionData {
    fn from(value: TracedExecutionData) -> Self {
        value.inner
    }
}

#[cfg(feature = "serde")]
impl serde::Serialize for TracedExecutionData {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        self.inner.serialize(serializer)
    }
}

#[cfg(feature = "serde")]
impl<'de> serde::Deserialize<'de> for TracedExecutionData {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        ExecutionData::deserialize(deserializer).map(Self::new)
    }
}
